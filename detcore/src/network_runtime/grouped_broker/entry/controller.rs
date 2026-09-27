//! Actual isolated startup owner. The private role header dispatches only
//! fixed maintained actor bodies; it carries no source or cleanup authority.
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use super::super::cleanup;
use super::super::keeper;
use super::super::serial;
use super::*;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Header {
    schema: String,
    role: String,
    nonce: String,
    incarnation: u64,
    stage_deadline: u64,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    schema: String,
    nonce: String,
    incarnation: u64,
    stage_deadline: u64,
    executable: String,
    bridge_sha256: String,
    namespace_setup_sha256: String,
    source_unit: String,
    leaf_unit: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeHandles {
    schema: String,
    nonce: String,
    incarnation: u64,
    stage_deadline: u64,
    keeper_pid: i32,
    keeper_uid: u32,
    keeper_gid: u32,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GuardianConfiguration {
    schema: String,
    nonce: String,
    incarnation: u64,
    stage_deadline: u64,
    unit: String,
    arguments: Vec<String>,
}

pub(super) fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut pair = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
            pair.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let left = unsafe { OwnedFd::from_raw_fd(pair[0]) };
    let right = unsafe { OwnedFd::from_raw_fd(pair[1]) };
    for fd in [left.as_raw_fd(), right.as_raw_fd()] {
        let one = 1i32;
        if unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                (&one as *const i32).cast(),
                4,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((left, right))
}
fn duplicate(fd: &OwnedFd) -> io::Result<OwnedFd> {
    let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}
fn directory(root: &OwnedFd, name: &str) -> io::Result<OwnedFd> {
    let root_identity = owner::stat(root.as_raw_fd())?;
    require(
        root_identity.mode & libc::S_IFMT == libc::S_IFDIR
            && root_identity.mode & 0o7777 == 0o700
            && root_identity.uid == unsafe { libc::getuid() },
        "startup original journal root differs",
    )?;
    require(
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'),
        "startup journal child name differs",
    )?;
    let name = CString::new(name).unwrap();
    if unsafe { libc::mkdirat(root.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let raw = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let held = unsafe { OwnedFd::from_raw_fd(raw) };
    require(
        unsafe { libc::fsync(root.as_raw_fd()) } == 0,
        "startup journal directory sync failed",
    )?;
    Ok(held)
}
fn peer(channel: &wire::Channel) -> io::Result<wire::Credentials> {
    channel.validate()?;
    let mut actual: libc::ucred = unsafe { std::mem::zeroed() };
    let mut n = std::mem::size_of_val(&actual) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            channel.fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut actual as *mut libc::ucred).cast(),
            &mut n,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    require(
        n as usize == std::mem::size_of_val(&actual)
            && actual.pid > 0
            && actual.pid != unsafe { libc::getpid() }
            && actual.uid == unsafe { libc::getuid() }
            && actual.gid == unsafe { libc::getgid() },
        "startup private peer differs",
    )?;
    Ok(wire::Credentials {
        pid: actual.pid,
        uid: actual.uid,
        gid: actual.gid,
    })
}
fn pause(deadline: Instant) -> io::Result<()> {
    require(
        Instant::now() < deadline,
        "startup original deadline expired",
    )?;
    std::thread::sleep(Duration::from_millis(1));
    Ok(())
}
fn receive(channel: &mut wire::Channel, deadline: Instant, cap: usize) -> io::Result<usize> {
    loop {
        require(
            Instant::now() < deadline,
            "startup receive original deadline expired",
        )?;
        if let Some(index) = channel.receive(cap)? {
            return Ok(index);
        }
        pause(deadline)?;
    }
}
fn header(role: &str, intent: &Intent, deadline: u64) -> io::Result<Vec<u8>> {
    journal::canonical(
        &json!({"schema":"hermit-grouped-startup-entry-v1","role":role,
        "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":deadline}),
    )
}
fn exact_header(
    parent: &mut wire::Channel,
    peer: wire::Credentials,
    intent: &Intent,
    deadline: Instant,
    native: u64,
) -> io::Result<Header> {
    let index = receive(parent, deadline, 4096)?;
    let packet = &parent.packets[index];
    packet.exact(0, peer)?;
    let actual: Header = serde_json::from_slice(&packet.bytes)?;
    require(
        journal::canonical(&serde_json::to_value(&actual)?)? == packet.bytes
            && actual.schema == "hermit-grouped-startup-entry-v1"
            && actual.nonce == intent.nonce
            && actual.incarnation == intent.incarnation
            && actual.stage_deadline == native
            && matches!(
                actual.role.as_str(),
                "controller" | "source-keeper" | "successor-guardian"
            ),
        "startup role header differs from original entry",
    )?;
    Ok(actual)
}
fn actor_command(executable: &str, intent: &Intent, deadline: u64, input: OwnedFd) -> Command {
    let mut command = Command::new(executable);
    command
        .args([
            "--grouped-startup-controller-private-stdin-v1",
            "--run",
            &intent.nonce,
            "--deadline-ns",
            &deadline.to_string(),
        ])
        .env_clear()
        .envs(
            crate::network_runtime::capability_unit::CAPABILITY_ENVIRONMENT
                .iter()
                .copied(),
        )
        .stdin(Stdio::from(input))
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
    command
}

/// # Safety
/// Called only by the exact early private CLI path before any worker/guest.
pub unsafe fn run_grouped_startup_controller_process(
    input: OwnedFd,
    run: [u8; 16],
    native_deadline: u64,
) -> ! {
    let mut parent = wire::Channel::retain(input);
    let result = (|| {
        protect()?;
        let start = Instant::now();
        let now = guardian::monotonic_ns()?;
        require(
            native_deadline > now && native_deadline - now <= 20_000_000_000,
            "startup entry original20s differs",
        )?;
        let deadline = start + Duration::from_nanos(native_deadline - now);
        let intent = Intent::new(
            super::super::hex(&run),
            u64::from_le_bytes(run[..8].try_into().unwrap()),
        )?;
        let original_peer = peer(&parent)?;
        let actual = exact_header(
            &mut parent,
            original_peer,
            &intent,
            deadline,
            native_deadline,
        )?;
        match actual.role.as_str() {
            "source-keeper" => run_keeper(parent, intent, deadline, native_deadline),
            "successor-guardian" => {
                run_guardian(parent, original_peer, intent, deadline, native_deadline)
            }
            "controller" => {
                run_controller(parent, original_peer, intent, deadline, native_deadline)
            }
            _ => unreachable!(),
        }
    })();
    finish_entry(result)
}

fn run_keeper(
    parent: wire::Channel,
    intent: Intent,
    deadline: Instant,
    native: u64,
) -> io::Result<()> {
    // The role packet remains retained in this actor until actual exit; the
    // actual channel descriptor moves once into the original Keeper owner.
    let role_receipts = parent.packets;
    let mut keeper = keeper::Keeper::retain(parent.fd, deadline, native);
    let result = (|| {
        keeper.initialize()?;
        keeper.receive_creation_install(intent.clone())?;
        keeper.install_source_bridge(&intent)?;
        loop {
            keeper.progress_successful_creation()?;
            if keeper.ready_for_success_exit() {
                return Ok(());
            }
            pause(deadline)?;
        }
    })();
    if let Err(error) = &result {
        match cleanup_cutoff(deadline, guardian::monotonic_ns().ok()) {
            Ok(cutoff) => loop {
                match keeper.retire_local_custody(cutoff, error) {
                    Ok(true) => {
                        if let Err(e) = owner::check_no_children() {
                            eprintln!("Keeper child join refused: {e}");
                        }
                        break;
                    }
                    Ok(false) => {
                        if let Err(e) = pause(cutoff) {
                            eprintln!("Keeper child join expired: {e}");
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("Keeper custody retirement refused: {e}");
                        break;
                    }
                }
            },
            Err(e) => eprintln!("Keeper original cleanup bound refused: {e}"),
        }
        std::mem::forget((keeper, role_receipts));
    }
    result
}

fn run_guardian(
    mut parent: wire::Channel,
    credentials: wire::Credentials,
    intent: Intent,
    deadline: Instant,
    native: u64,
) -> io::Result<()> {
    let mut holder = None;
    let result = (|| {
        let index = receive(&mut parent, deadline, 65_536)?;
        let packet = &mut parent.packets[index];
        packet.exact(2, credentials)?;
        let config: GuardianConfiguration = serde_json::from_slice(&packet.bytes)?;
        require(
            journal::canonical(&serde_json::to_value(&config)?)? == packet.bytes
                && config.schema == "hermit-grouped-successor-guardian-config-v1"
                && config.nonce == intent.nonce
                && config.incarnation == intent.incarnation
                && config.stage_deadline == native,
            "S2 Guardian configuration differs",
        )?;
        let arguments = config.arguments.iter().map(OsString::from).collect();
        let (local, remote) = pair()?;
        holder = Some(guardian::Holder::retain(
            intent.clone(),
            config.unit,
            deadline,
            native,
            guardian::Role::Guardian,
            local,
            packet.rights.remove(1),
            packet.rights.remove(0),
            arguments,
        ));
        holder.as_mut().unwrap().initialize()?;
        parent.send_once(
            &journal::canonical(
                &json!({"schema":"hermit-grouped-successor-guardian-channel-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":native}),
            )?,
            &[remote.as_fd()],
        )?;
        drop(remote);
        while !holder.as_ref().unwrap().controls_ready() {
            holder.as_mut().unwrap().progress()?;
            pause(deadline)?;
        }
        // This actor authenticated adoption controls, not a source17 journal.
        // Do not consume peer EOF through source-completion code after handoff.
        let index = receive(&mut parent, deadline, 4096)?;
        let packet = &parent.packets[index];
        packet.exact(0, credentials)?;
        let expected = json!({"schema":"hermit-grouped-successor-guardian-release-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":native});
        require(
            packet.bytes == journal::canonical(&expected)?,
            "S2 Guardian release is not the original controller",
        )?;
        owner::check_no_children()?;
        parent.send_once(
            &journal::canonical(
                &json!({"schema":"hermit-grouped-successor-guardian-released-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":native}),
            )?,
            &[],
        )?;
        Ok(())
    })();
    if let Err(error) = &result {
        if let Some(holder) = &mut holder {
            match cleanup_cutoff(deadline, guardian::monotonic_ns().ok()) {
                Ok(cutoff) => loop {
                    match holder.retire_local_custody(cutoff, error) {
                        Ok(true) => {
                            if let Err(e) = owner::check_no_children() {
                                eprintln!("Guardian child join refused: {e}");
                            }
                            break;
                        }
                        Ok(false) => {
                            if let Err(e) = pause(cutoff) {
                                eprintln!("Guardian child join expired: {e}");
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!("Guardian custody retirement refused: {e}");
                            break;
                        }
                    }
                },
                Err(e) => eprintln!("Guardian original cleanup bound refused: {e}"),
            }
        }
        std::mem::forget((parent, holder));
    }
    result
}

fn cleanup_cutoff(stage: Instant, origin: Option<u64>) -> io::Result<Instant> {
    let origin = origin.ok_or_else(|| io::Error::other("original cleanup origin unknown"))?;
    let sampled = Instant::now();
    let now = guardian::monotonic_ns()?;
    require(
        now >= origin && now - origin < 1_000_000_000,
        "original cleanup1s expired or future",
    )?;
    let cutoff = stage.min(sampled + Duration::from_nanos(1_000_000_000 - (now - origin)));
    require(Instant::now() < cutoff, "original cleanup stage expired")?;
    Ok(cutoff)
}

// The controller body follows below; all fallible operations borrow its one
// installed retained state, including source/leaf/S2 acquisition failures.
struct Controller {
    parent: wire::Channel,
    original_peer: wire::Credentials,
    intent: Intent,
    deadline: Instant,
    native: u64,
    configuration: Option<Configuration>,
    image: Option<OwnedFd>,
    bridge: Option<OwnedFd>,
    root: Option<OwnedFd>,
    namespace_setup: Option<OwnedFd>,
    prepared_namespace: Option<owner::PreparedLeafNamespace>,
    provider: Option<serial::successor::ProviderIdentity>,
    service_peer: Option<wire::Credentials>,
    runtime: Option<runtime_creation::RuntimeCreationCustody>,
    source: Option<cleanup::startup::PreparedSource>,
    leaf: Option<owner::LeafDelegate>,
    pending_child: Option<owner::Launcher>,
    s2: Option<wire::Channel>,
    guardian: Option<owner::Launcher>,
    guardian_parent: Option<wire::Channel>,
    guardian_endpoint: Option<OwnedFd>,
    guardian_logs: Option<OwnedFd>,
    successor: Option<serial::successor::RetainedSuccessor>,
    completed: Option<serial::CompletedSourceExport>,
    first_failure: Option<super::super::Failure>,
    first_failure_origin: Option<u64>,
    guardian_shutdown: guardian::CustodyShutdown,
    custody_report_attempted: bool,
    custody_report: Option<serde_json::Value>,
    custody_report_bytes: Vec<u8>,
    custody_report_file: Option<OwnedFd>,
    custody_report_readonly: Option<OwnedFd>,
}
impl Controller {
    fn retain(
        parent: wire::Channel,
        original_peer: wire::Credentials,
        intent: Intent,
        deadline: Instant,
        native: u64,
    ) -> Self {
        Self {
            parent,
            original_peer,
            intent,
            deadline,
            native,
            configuration: None,
            image: None,
            bridge: None,
            root: None,
            namespace_setup: None,
            prepared_namespace: None,
            provider: None,
            service_peer: None,
            runtime: None,
            source: None,
            leaf: None,
            pending_child: None,
            s2: None,
            guardian: None,
            guardian_parent: None,
            guardian_endpoint: None,
            guardian_logs: None,
            successor: None,
            completed: None,
            first_failure: None,
            first_failure_origin: None,
            guardian_shutdown: guardian::CustodyShutdown::default(),
            custody_report_attempted: false,
            custody_report: None,
            custody_report_bytes: Vec::new(),
            custody_report_file: None,
            custody_report_readonly: None,
        }
    }
    fn check(&self) -> io::Result<()> {
        if let Some(error) = &self.first_failure {
            return Err(error.error());
        }
        require(
            Instant::now() < self.deadline && guardian::monotonic_ns()? < self.native,
            "controller original startup deadline expired",
        )
    }
    fn configure(&mut self) -> io::Result<()> {
        let index = receive(&mut self.parent, self.deadline, 65_536)?;
        let packet = &mut self.parent.packets[index];
        packet.exact(3, self.original_peer)?;
        let config: Configuration = serde_json::from_slice(&packet.bytes)?;
        require(
            journal::canonical(&serde_json::to_value(&config)?)? == packet.bytes
                && config.schema == "hermit-grouped-startup-controller-config-v2"
                && config.nonce == self.intent.nonce
                && config.incarnation == self.intent.incarnation
                && config.stage_deadline == self.native,
            "controller configuration changed original entry",
        )?;
        for unit in [&config.source_unit, &config.leaf_unit] {
            require(
                unit.strip_prefix("hermit-accepted-")
                    .and_then(|s| s.strip_suffix(".service"))
                    .is_some_and(super::super::valid_nonce),
                "controller unit nonce is malformed",
            )?;
        }
        require(
            config.source_unit != config.leaf_unit
                && PathBuf::from(&config.executable).is_absolute()
                && !config.executable.as_bytes().contains(&0),
            "controller launch configuration aliases units or executable",
        )?;
        decode_digest(&config.bridge_sha256)?;
        decode_digest(&config.namespace_setup_sha256)?;
        self.image = Some(packet.rights.remove(0));
        self.bridge = Some(packet.rights.remove(0));
        self.root = Some(packet.rights.remove(0));
        self.configuration = Some(config);
        let actual: OwnedFd = std::fs::File::open(std::env::current_exe()?)?.into();
        require(
            owner::stat(actual.as_raw_fd())?
                .same_object(&owner::stat(self.image.as_ref().unwrap().as_raw_fd())?),
            "controller held image differs from actual executing Hermit",
        )?;
        let named: OwnedFd =
            std::fs::File::open(&self.configuration.as_ref().unwrap().executable)?.into();
        require(
            owner::stat(named.as_raw_fd())?.same_object(&owner::stat(actual.as_raw_fd())?),
            "controller launch path differs from held executing image",
        )?;
        drop((actual, named));
        let setup_index = receive(&mut self.parent, self.deadline, 4096)?;
        let setup_packet = &mut self.parent.packets[setup_index];
        setup_packet.exact(1, self.original_peer)?;
        require(
            setup_packet.bytes
                == journal::canonical(
                    &json!({"schema":"hermit-grouped-controller-setup-image-v1",
            "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"stage_deadline":self.native}),
                )?,
            "controller setup image transfer changed original startup",
        )?;
        self.namespace_setup = Some(setup_packet.rights.remove(0));
        let capture_index = receive(&mut self.parent, self.deadline, 65_536)?;
        let image_index = receive(&mut self.parent, self.deadline, 4096)?;
        let (left, right) = self.parent.packets.split_at_mut(image_index);
        self.provider = Some(serial::successor::ProviderIdentity::retain(
            &mut left[capture_index],
            &mut right[0],
            self.original_peer,
            &self.intent.nonce,
            self.native,
        )?);
        let provider = self.provider.as_mut().unwrap();
        let unit = provider.unit().to_owned();
        let arguments = provider.arguments();
        require(
            unit != self.configuration.as_ref().unwrap().source_unit
                && unit != self.configuration.as_ref().unwrap().leaf_unit,
            "provider/source/leaf units must be distinct original launches",
        )?;
        provider.initialize(&unit, &arguments, &self.intent.nonce, self.native)?;
        self.service_peer = Some(provider.peer());
        let index = receive(&mut self.parent, self.deadline, 4096)?;
        let packet = &mut self.parent.packets[index];
        packet.exact(2, self.original_peer)?;
        let handles: RuntimeHandles = serde_json::from_slice(&packet.bytes)?;
        require(
            journal::canonical(&serde_json::to_value(&handles)?)? == packet.bytes
                && handles.schema == "hermit-grouped-controller-runtime-v1"
                && handles.nonce == self.intent.nonce
                && handles.incarnation == self.intent.incarnation
                && handles.stage_deadline == self.native,
            "controller runtime handles replaced original startup",
        )?;
        self.runtime = Some(runtime_creation::RuntimeCreationCustody::retain(
            packet.rights.remove(0),
            packet.rights.remove(0),
            wire::Credentials {
                pid: handles.keeper_pid,
                uid: handles.keeper_uid,
                gid: handles.keeper_gid,
            },
            self.intent.clone(),
            self.native,
        ));
        // This pair must originate in the actual controller, never in the CLI
        // wrapper: native C authenticates its Keeper using SO_PEERCRED.
        let (local, remote) = pair()?;
        self.s2 = Some(wire::Channel::retain(local));
        self.parent.send_once(&journal::canonical(&json!({"schema":"hermit-grouped-successor-channel-v1",
            "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"stage_deadline":self.native}))?,&[remote.as_fd()])?;
        drop(remote);
        self.check()
    }
    fn arguments(&self, flag: &str, unit: &str) -> Vec<OsString> {
        [
            flag.to_owned(),
            "--unit".to_owned(),
            unit.to_owned(),
            "--run".to_owned(),
            self.intent.nonce.clone(),
            "--incarnation".to_owned(),
            self.intent.incarnation.to_string(),
            "--deadline-ns".to_owned(),
            self.native.to_string(),
        ]
        .into_iter()
        .map(Into::into)
        .collect()
    }
    fn source(&mut self) -> io::Result<()> {
        self.check()?;
        let config = self.configuration.as_ref().unwrap();
        let executable = config.executable.clone();
        let unit = config.source_unit.clone();
        let digest = decode_digest(&config.bridge_sha256)?;
        let arguments = self.arguments("--grouped-source-private-stdin-v1", &unit);
        let mut expected = vec![OsString::from(&executable)];
        expected.extend(arguments.clone());
        let (parent, child) = pair()?;
        let (guardian_channel, guardian_endpoint) = pair()?;
        let holder = guardian::Holder::retain(
            self.intent.clone(),
            unit.clone(),
            self.deadline,
            self.native,
            guardian::Role::Guardian,
            guardian_channel,
            directory(self.root.as_ref().unwrap(), "source-guardian")?,
            duplicate(self.image.as_ref().unwrap())?,
            expected.clone(),
        );
        self.source = Some(cleanup::startup::PreparedSource::retain(
            holder,
            duplicate(self.bridge.as_ref().unwrap())?,
            digest,
            parent,
            directory(self.root.as_ref().unwrap(), "source-guardian-cleanup")?,
            directory(self.root.as_ref().unwrap(), "source-logs")?,
            directory(self.root.as_ref().unwrap(), "source-keeper-logs")?,
            self.runtime
                .take()
                .ok_or_else(|| io::Error::other("outside runtime owner absent"))?,
        ));
        self.source.as_mut().unwrap().prepare()?;
        let mut command = actor_command(&executable, &self.intent, self.native, child);
        self.pending_child = Some(owner::Launcher::retain(command.spawn()?));
        drop(command);
        self.source
            .as_mut()
            .unwrap()
            .install_keeper(&mut self.pending_child)?;
        self.source
            .as_mut()
            .unwrap()
            .parent()
            .send_once(&header("source-keeper", &self.intent, self.native)?, &[])?;
        let keeper_cleanup = directory(self.root.as_ref().unwrap(), "source-keeper-cleanup")?;
        self.source.as_mut().unwrap().parent().send_once(&journal::canonical(&json!({"schema":"hermit-cleanup-install-v1",
            "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"stage_deadline":self.native}))?,&[keeper_cleanup.as_fd()])?;
        let bridge_frame = BridgeConfiguration {
            schema: "hermit-grouped-source-bridge-v1".to_owned(),
            nonce: self.intent.nonce.clone(),
            incarnation: self.intent.incarnation,
            unit: unit.clone(),
            stage_deadline: self.native,
            bridge_sha256: self.configuration.as_ref().unwrap().bridge_sha256.clone(),
        };
        self.source.as_mut().unwrap().parent().send_once(
            &journal::canonical(&serde_json::to_value(&bridge_frame)?)?,
            &[self.bridge.as_ref().unwrap().as_fd()],
        )?;
        let keeper_store = directory(self.root.as_ref().unwrap(), "source-keeper")?;
        self.source.as_mut().unwrap().parent().send_once(&journal::canonical(&json!({"schema":"hermit-grouped-keeper-config-v1",
            "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"unit":unit,"stage_deadline":self.native,
            "arguments":expected.iter().map(|s|super::super::hex(s.as_bytes())).collect::<Vec<_>>()}))?,
            &[self.image.as_ref().unwrap().as_fd(),guardian_endpoint.as_fd(),keeper_store.as_fd()])?;
        drop((keeper_cleanup, keeper_store, guardian_endpoint));
        let index = loop {
            let source = self.source.as_mut().unwrap();
            source.drain()?;
            if let Some(index) = source.parent().receive(2048)? {
                break index;
            }
            pause(self.deadline)?;
        };
        let source = self.source.as_mut().unwrap();
        let peer = source.keeper_peer();
        let packet = &mut source.parent().packets[index];
        packet.exact(1, peer)?;
        require(
            packet.bytes
                == journal::canonical(&json!({"schema":"hermit-grouped-source-channel-v1",
            "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"stage_deadline":self.native}))?,
            "actual source channel differs",
        )?;
        let mut input = Some(packet.rights.remove(0));
        source.launch_source(
            &unit,
            &executable,
            &expected,
            &mut input,
            self.image.as_ref().unwrap().as_fd(),
        )?;
        while !self.source.as_mut().unwrap().receive_source_launch()? {
            pause(self.deadline)?;
        }
        let source = self.source.as_mut().unwrap();
        let wrapper = source.source_pid();
        let pidfd = source.source_pidfd().try_clone_to_owned()?;
        source.parent().send_once(&journal::canonical(&json!({"schema":"hermit-grouped-source-launched-v1",
            "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"stage_deadline":self.native,"wrapper":wrapper}))?,&[pidfd.as_fd()])?;
        drop(pidfd);
        while !self.source.as_mut().unwrap().progress_creation()? {
            pause(self.deadline)?;
        }
        self.source.as_mut().unwrap().begin_serial_join()?;
        while !self.source.as_mut().unwrap().progress_serial()? {
            pause(self.deadline)?;
        }
        self.check()
    }
    fn leaves(&mut self) -> io::Result<()> {
        self.check()?;
        let config = self.configuration.as_ref().unwrap();
        let unit = config.leaf_unit.clone();
        let executable = config.executable.clone();
        let arguments = self.arguments("--grouped-leaves-private-stdin-v1", &unit);
        let mut expected = vec![OsString::from(&executable)];
        expected.extend(arguments.clone());
        let (local, remote) = pair()?;
        self.leaf = Some(owner::LeafDelegate::retain(
            self.intent.clone(),
            unit.clone(),
            self.deadline,
            self.native,
            duplicate(self.image.as_ref().unwrap())?,
            expected,
            local,
            directory(self.root.as_ref().unwrap(), "leaf-logs")?,
        ));
        // Hash the original held expected image before starting the helper's
        // one-second Creator interval. The original outer stage still applies.
        self.leaf.as_mut().unwrap().prepare_image()?;
        self.prepared_namespace = Some(self.source.as_mut().unwrap().take_leaf_namespace()?);
        self.leaf.as_mut().unwrap().install_preparation(
            &mut self.prepared_namespace,
            &mut self.namespace_setup,
            decode_digest(&self.configuration.as_ref().unwrap().namespace_setup_sha256)?,
        )?;
        let launch = crate::network_runtime::capability_unit::CapabilityUnitLaunch {
            kind: crate::network_runtime::capability_unit::CapabilityServiceKind::Accepted,
            unit: &unit,
            executable: std::path::Path::new(&executable),
            arguments: &arguments,
            lifetime: crate::network_runtime::capability_unit::CapabilityServiceLifetime::Bounded(
                20,
            ),
            writable_directories: &[],
        };
        let (namespace, setup, image) = self.leaf.as_ref().unwrap().preparation()?;
        let ns = namespace.identity();
        let user = namespace.owning_userns();
        let root = namespace.root();
        let argv = launch.arguments_with_prepared_namespace(
            &self.intent.nonce,
            namespace.fd().as_raw_fd(),
            setup,
            image,
            (ns.device, ns.inode),
            (user.device, user.inode),
            (root.device, root.inode),
        )?;
        let mut command = Command::new(crate::network_runtime::capability_unit::CAPABILITY_SUDO);
        command
            .env_clear()
            .envs(
                crate::network_runtime::capability_unit::CAPABILITY_ENVIRONMENT
                    .iter()
                    .copied(),
            )
            .args(argv)
            .stdin(Stdio::from(remote))
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
        self.pending_child = Some(owner::Launcher::retain(command.spawn()?));
        drop(command);
        self.leaf
            .as_mut()
            .unwrap()
            .install_launcher(&mut self.pending_child)?;
        self.leaf.as_mut().unwrap().initialize()?;
        while !self.leaf.as_mut().unwrap().progress()? {
            pause(self.deadline)?;
        }
        self.check()
    }
    fn successor(&mut self) -> io::Result<()> {
        self.check()?;
        let provider = self.provider.as_ref().unwrap();
        let unit = provider.unit().to_owned();
        let arguments = provider.arguments();
        self.source
            .as_mut()
            .unwrap()
            .prepare_leaf_plan(serial::SuccessorRequest::retain(
                unit.clone(),
                arguments.clone(),
            ))?;
        let (parent, child) = pair()?;
        self.guardian_parent = Some(wire::Channel::retain(parent));
        self.guardian_logs = Some(directory(
            self.root.as_ref().unwrap(),
            "successor-guardian-logs",
        )?);
        let mut command = actor_command(
            &self.configuration.as_ref().unwrap().executable,
            &self.intent,
            self.native,
            child,
        );
        self.guardian = Some(owner::Launcher::retain(command.spawn()?));
        drop(command);
        self.guardian
            .as_mut()
            .unwrap()
            .initialize(self.guardian_logs.as_ref().unwrap().as_raw_fd())?;
        self.source
            .as_mut()
            .unwrap()
            .begin_s2_guardian(self.guardian.as_ref().unwrap())?;
        while !self.source.as_mut().unwrap().receive_s2_guardian_ack()? {
            self.guardian.as_mut().unwrap().drain()?;
            pause(self.deadline)?;
        }
        self.guardian_parent.as_mut().unwrap().send_once(
            &header("successor-guardian", &self.intent, self.native)?,
            &[],
        )?;
        let guardian_store = directory(self.root.as_ref().unwrap(), "successor-guardian")?;
        let guardian_config = GuardianConfiguration {
            schema: "hermit-grouped-successor-guardian-config-v1".to_owned(),
            nonce: self.intent.nonce.clone(),
            incarnation: self.intent.incarnation,
            stage_deadline: self.native,
            unit,
            arguments: arguments
                .iter()
                .map(|s| {
                    s.to_str()
                        .map(str::to_owned)
                        .ok_or_else(|| io::Error::other("provider launch argv is not UTF8"))
                })
                .collect::<io::Result<_>>()?,
        };
        self.guardian_parent.as_mut().unwrap().send_once(
            &journal::canonical(&serde_json::to_value(&guardian_config)?)?,
            &[self.image.as_ref().unwrap().as_fd(), guardian_store.as_fd()],
        )?;
        drop(guardian_store);
        let index = loop {
            self.guardian.as_mut().unwrap().drain()?;
            if let Some(index) = self.guardian_parent.as_mut().unwrap().receive(4096)? {
                break index;
            }
            pause(self.deadline)?;
        };
        let packet = &mut self.guardian_parent.as_mut().unwrap().packets[index];
        packet.exact(
            1,
            wire::Credentials {
                pid: self.guardian.as_ref().unwrap().child.id() as i32,
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
            },
        )?;
        require(
            packet.bytes
                == journal::canonical(
                    &json!({"schema":"hermit-grouped-successor-guardian-channel-v1",
            "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"stage_deadline":self.native}),
                )?,
            "S2 Guardian original channel differs",
        )?;
        self.guardian_endpoint = Some(packet.rights.remove(0));
        let cutoff = guardian::monotonic_ns()?
            .checked_add(1_000_000_000)
            .ok_or_else(|| io::Error::other("S2 original cutoff overflow"))?
            .min(self.native);
        self.leaf
            .as_mut()
            .unwrap()
            .deliver(self.s2.as_mut().unwrap(), cutoff)?;
        while !self
            .leaf
            .as_mut()
            .unwrap()
            .receive_delivery_ack(self.s2.as_mut().unwrap(), self.service_peer.unwrap())?
        {
            self.guardian.as_mut().unwrap().drain()?;
            pause(self.deadline)?;
        }
        let store = directory(self.root.as_ref().unwrap(), "successor-keeper")?;
        self.successor = Some(serial::successor::RetainedSuccessor::retain(
            self.source.as_mut().unwrap().take_serial()?,
            self.s2.take().unwrap().fd,
            self.guardian_endpoint.take().unwrap(),
            self.guardian.take().unwrap(),
            self.provider.take().unwrap(),
            store,
            cutoff,
            self.intent.clone(),
        ));
        self.successor.as_mut().unwrap().initialize()?;
        while !self.successor.as_ref().unwrap().acknowledgement_sent() {
            self.successor.as_mut().unwrap().progress()?;
            pause(self.deadline)?;
        }
        self.completed = Some(serial::CompletedSourceExport::retain(
            self.successor.take().unwrap(),
        ));
        self.completed.as_mut().unwrap().prepare()?;
        while self.completed.as_mut().unwrap().send_next()? {
            self.check()?;
        }
        while !self
            .completed
            .as_mut()
            .unwrap()
            .receive_dual_custody_ack()?
        {
            pause(self.deadline)?;
        }
        self.completed.as_mut().unwrap().finish_startup(
            self.guardian_parent.as_mut().unwrap(),
            self.guardian_logs.as_ref().unwrap().as_fd(),
        )?;
        self.check()
    }
    fn drive(&mut self) -> io::Result<()> {
        self.configure()?;
        self.source()?;
        self.leaves()?;
        self.successor()
    }
    fn retire_local_custody(&mut self, cutoff: Instant, error: &io::Error) -> io::Result<bool> {
        if let Some(completed) = &mut self.completed {
            return completed.retire_local_custody(
                self.guardian_logs.as_ref().unwrap().as_fd(),
                cutoff,
                error,
            );
        }
        if let Some(successor) = &mut self.successor {
            return successor.retire_local_custody(
                self.guardian_logs.as_ref().unwrap().as_fd(),
                cutoff,
                error,
            );
        }
        if let Some(guardian) = &mut self.guardian {
            // The S2 launcher is installed before its initialization/config;
            // S1 already joined before that spawn. Retire this actual Child
            // even if the successor owner was never constructed.
            return Ok(matches!(
                guardian.retire_custody(
                    self.guardian_logs.as_ref().unwrap().as_raw_fd(),
                    cutoff,
                    error
                )?,
                owner::QueryRetirement::NoChild | owner::QueryRetirement::Retired
            ));
        }
        if let Some(source) = &mut self.source {
            return source.retire_local_custody(cutoff, error);
        }
        require(
            self.pending_child.is_none(),
            "uninstalled original Child requires retained launcher retirement",
        )?;
        owner::check_no_children()?;
        Ok(true)
    }
    fn finish_local_custody(&mut self, cutoff: Instant, error: &io::Error) -> io::Result<()> {
        loop {
            if self.retire_local_custody(cutoff, error)? {
                return Ok(());
            }
            pause(cutoff)?;
        }
    }
    fn local_custody_records(&self) -> io::Result<Vec<serde_json::Value>> {
        let mut records = if let Some(completed) = &self.completed {
            completed.local_custody_records()?
        } else if let Some(successor) = &self.successor {
            successor.local_custody_records()?
        } else if let Some(source) = &self.source {
            source.local_custody_records()?
        } else {
            Vec::new()
        };
        if let Some(guardian) = &self.guardian {
            records.push(json!({"kind":"launcher","role":"successor-guardian","record":guardian.custody_evidence()}));
        }
        require(
            self.pending_child.is_none(),
            "controller has uninstalled Child custody",
        )?;
        if let Some(leaf) = &self.leaf {
            let leaf = leaf.diagnostics();
            require(
                leaf["custody_retired"] == true,
                "Leaf custody was not retired",
            )?;
            for (name, kind) in [
                ("launcher", "launcher"),
                ("manager", "query"),
                ("entry", "query"),
                ("named", "query"),
                ("terminal", "query"),
                ("stop", "query"),
                ("cleanup_query", "query"),
                ("cleanup_stop", "query"),
                ("cleanup_forget", "query"),
            ] {
                require(
                    leaf.get(name).is_some(),
                    "Leaf custody report lacks an original query slot",
                )?;
                if !leaf[name].is_null() {
                    records.push(
                        json!({"kind":kind,"role":format!("leaf-{name}"),"record":leaf[name]}),
                    );
                }
            }
        }
        Ok(records)
    }
    fn emit_local_custody(&mut self, cutoff: Instant) -> io::Result<()> {
        use sha2::Digest;
        use sha2::Sha256;
        require(
            !self.custody_report_attempted && Instant::now() < cutoff,
            "controller custody report repeated or expired",
        )?;
        self.custody_report_attempted = true;
        let origin = if let Some(owner) = &self.completed {
            owner.failure_notice_origin()?
        } else if let Some(owner) = &self.successor {
            owner.failure_notice_origin()?
        } else if let Some(owner) = &self.source {
            owner.failure_notice_origin()?
        } else {
            self.runtime
                .as_ref()
                .ok_or_else(|| io::Error::other("original runtime custody absent"))?
                .failure_notice_origin()?
        };
        let records = self.local_custody_records()?;
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let raw = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        let errno = if raw == -1 {
            io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        let report = json!({"schema":"hermit-grouped-controller-local-custody-report-v1","nonce":self.intent.nonce,
            "incarnation":self.intent.incarnation,"stage_deadline":self.native,"first_failure_origin":origin,
            "records":records,"launcher_count":records.iter().filter(|r|r["kind"]=="launcher"&&r["record"]["pid"].is_number()).count(),
            "query_count":records.iter().filter(|r|r["kind"]=="query"&&r["record"]["pid"].is_number()).count(),
            "waitid":{"idtype":"P_ALL","options":["WEXITED","WNOHANG","WNOWAIT"],"returned":raw,"errno":errno}});
        self.custody_report = Some(report);
        require(
            raw == -1 && errno == Some(libc::ECHILD),
            "controller local custody lacks actual global ECHILD",
        )?;
        self.custody_report_bytes = journal::canonical(self.custody_report.as_ref().unwrap())?;
        require(
            self.custody_report_bytes.len() <= 1_048_576,
            "controller custody report exceeds original byte bound",
        )?;
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| io::Error::other("controller original receipt root absent"))?;
        let raw = unsafe {
            libc::openat(
                root.as_raw_fd(),
                c"controller-local-custody.json".as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if raw == -1 {
            return Err(io::Error::last_os_error());
        }
        self.custody_report_file = Some(unsafe { OwnedFd::from_raw_fd(raw) });
        let mut offset = 0;
        while offset < self.custody_report_bytes.len() {
            require(
                Instant::now() < cutoff,
                "controller custody report exceeded original cutoff",
            )?;
            let raw = unsafe {
                libc::pwrite(
                    self.custody_report_file.as_ref().unwrap().as_raw_fd(),
                    self.custody_report_bytes[offset..].as_ptr().cast(),
                    self.custody_report_bytes.len() - offset,
                    offset as i64,
                )
            };
            if raw == -1 {
                return Err(io::Error::last_os_error());
            }
            require(raw > 0, "controller custody report write stalled")?;
            offset += raw as usize;
        }
        for fd in [
            self.custody_report_file.as_ref().unwrap().as_raw_fd(),
            root.as_raw_fd(),
        ] {
            if unsafe { libc::fsync(fd) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let raw = unsafe {
            libc::openat(
                root.as_raw_fd(),
                c"controller-local-custody.json".as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw == -1 {
            return Err(io::Error::last_os_error());
        }
        self.custody_report_readonly = Some(unsafe { OwnedFd::from_raw_fd(raw) });
        let file = self.custody_report_readonly.as_ref().unwrap();
        require(
            owner::stat(file.as_raw_fd())?.same_object(&owner::stat(
                self.custody_report_file.as_ref().unwrap().as_raw_fd(),
            )?),
            "controller custody report name replaced original held file",
        )?;
        let mut readback = vec![0u8; self.custody_report_bytes.len() + 1];
        let raw = unsafe {
            libc::pread(
                file.as_raw_fd(),
                readback.as_mut_ptr().cast(),
                readback.len(),
                0,
            )
        };
        if raw == -1 {
            return Err(io::Error::last_os_error());
        }
        readback.truncate(raw as usize);
        require(
            readback == self.custody_report_bytes
                && Sha256::digest(&readback) == Sha256::digest(&self.custody_report_bytes),
            "controller custody report actual bytes differ",
        )?;
        require(
            Instant::now() < cutoff,
            "controller custody report exceeded original cutoff",
        )?;
        let report = self.custody_report.as_ref().unwrap();
        if let Some(owner) = &mut self.completed {
            owner.send_local_custody(report, file.as_fd())
        } else if let Some(owner) = &mut self.successor {
            owner.send_local_custody(report, file.as_fd())
        } else if let Some(owner) = &mut self.source {
            owner.send_local_custody(report, file.as_fd())
        } else {
            self.runtime
                .as_mut()
                .unwrap()
                .send_local_custody(report, file.as_fd())
        }
    }
    fn refuse(&mut self, error: &io::Error) {
        if self.first_failure.is_none() {
            self.first_failure = Some(super::super::Failure::capture(error));
            self.first_failure_origin = guardian::monotonic_ns().ok();
        }
        let notification = if let Some(completed) = &mut self.completed {
            completed.notify_runtime_failure(self.first_failure_origin, error)
        } else if let Some(successor) = &mut self.successor {
            successor.notify_runtime_failure(self.first_failure_origin, error)
        } else if let Some(source) = &mut self.source {
            source.notify_runtime_failure(self.first_failure_origin, error)
        } else if let Some(runtime) = &mut self.runtime {
            runtime.notify_failure(self.first_failure_origin, error)
        } else {
            Ok(())
        };
        if let Err(secondary) = notification {
            eprintln!("startup original runtime failure notification refused: {secondary}");
        }
        if let Ok(cutoff) = cleanup_cutoff(self.deadline, self.first_failure_origin) {
            let later_guardian =
                self.guardian.is_some() || self.successor.is_some() || self.completed.is_some();
            if let Some(parent) = &self.guardian_parent {
                if let Err(e) = self.guardian_shutdown.progress(parent) {
                    eprintln!("Guardian writer retirement refused: {e}");
                }
            }
            // The Leaf's original successful join predates S2's actual Child.
            // Finish S2 first before asking the retained Leaf to recheck global
            // ECHILD. Before S2, the only possible live local actor is the Leaf
            // itself (S1 joined before it was spawned).
            let mut local_complete = true;
            if later_guardian || self.leaf.is_none() {
                if let Err(e) = self.finish_local_custody(cutoff, error) {
                    local_complete = false;
                    eprintln!("controller local custody refused: {e}");
                }
            }
            if let Some(leaf) = &mut self.leaf {
                leaf.refuse(error);
                loop {
                    match leaf.retire_custody(cutoff) {
                        Ok(true) => break,
                        Ok(false) => {
                            if let Err(e) = pause(cutoff) {
                                local_complete = false;
                                eprintln!("leaf custody expired: {e}");
                                break;
                            }
                        }
                        Err(e) => {
                            local_complete = false;
                            eprintln!("leaf custody retirement refused: {e}");
                            break;
                        }
                    }
                }
            }
            if !later_guardian && self.leaf.is_some() {
                if let Err(e) = self.finish_local_custody(cutoff, error) {
                    local_complete = false;
                    eprintln!("controller local custody refused: {e}");
                }
            }
            if let Err(e) = owner::check_no_children() {
                local_complete = false;
                eprintln!("controller final local child census refused: {e}");
            }
            if local_complete {
                if let Err(e) = self.emit_local_custody(cutoff) {
                    eprintln!("controller durable custody receipt refused: {e}");
                }
            }
        }
        eprintln!("startup controller retained failure: {error}");
    }
}
fn run_controller(
    parent: wire::Channel,
    peer: wire::Credentials,
    intent: Intent,
    deadline: Instant,
    native: u64,
) -> io::Result<()> {
    let mut owner = Controller::retain(parent, peer, intent, deadline, native);
    let result = owner.drive();
    if let Err(error) = &result {
        owner.refuse(error);
        std::mem::forget(owner);
    }
    result
}
