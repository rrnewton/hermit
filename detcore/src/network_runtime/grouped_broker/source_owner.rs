//! Fixed ordinary source manager owner. Existing capability services retain
//! their NoNewPrivileges and exact capability policy; this process performs
//! only the original owner-authorized source launch and terminal manager work.
use std::ffi::CString;
use std::ffi::OsString;
use std::io;
use std::mem::ManuallyDrop;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use super::Failure;
use super::Intent;
use super::guardian;
use super::hex;
use super::journal;
use super::owner;
use super::parent_launch::ParentLaunchCustody;
use super::require;
use super::runtime_cleanup as common;
use super::wire;

pub(super) struct OwnedSource {
    request: Value,
    rights: Vec<OwnedFd>,
    directory: Option<OwnedFd>,
    input_alias: Option<OwnedFd>,
    input_close: Option<(i32, Option<i32>)>,
    launcher: Option<owner::Launcher>,
    capture: Option<ParentLaunchCustody>,
    retirement: Option<owner::OutsideSourceRetirement>,
    capture_observation: Option<Value>,
    log_readers: Vec<OwnedFd>,
    terminal: Option<Value>,
    sent_terminal: bool,
    launch_attempted: bool,
}
impl OwnedSource {
    fn retain(request: Value, rights: Vec<OwnedFd>) -> Self {
        Self {
            request,
            rights,
            directory: None,
            input_alias: None,
            input_close: None,
            launcher: None,
            capture: None,
            retirement: None,
            capture_observation: None,
            log_readers: Vec::new(),
            terminal: None,
            sent_terminal: false,
            launch_attempted: false,
        }
    }
    fn drain(&mut self) -> io::Result<()> {
        if let Some(launcher) = &mut self.launcher {
            launcher.drain()?;
        }
        Ok(())
    }
    fn spawn(&mut self, intent: &Intent, stage: u64, receipts: BorrowedFd<'_>) -> io::Result<()> {
        common::before(stage)?;
        require(
            !self.launch_attempted && self.rights.len() == 2,
            "outside source launch absent or repeated",
        )?;
        let unit = self.request["unit"]
            .as_str()
            .ok_or_else(|| io::Error::other("outside source unit absent"))?
            .to_owned();
        let nonce = unit
            .strip_prefix("hermit-accepted-")
            .and_then(|n| n.strip_suffix(".service"))
            .filter(|n| super::valid_nonce(n))
            .ok_or_else(|| io::Error::other("outside source unit nonce malformed"))?
            .to_owned();
        let executable = self.request["executable"]
            .as_str()
            .ok_or_else(|| io::Error::other("outside source executable absent"))?
            .to_owned();
        require(
            std::path::Path::new(&executable).is_absolute() && !executable.as_bytes().contains(&0),
            "outside source executable must be exact absolute image",
        )?;
        let arguments = vec![
            "--grouped-source-private-stdin-v1".to_owned(),
            "--unit".to_owned(),
            unit.clone(),
            "--run".to_owned(),
            intent.nonce.clone(),
            "--incarnation".to_owned(),
            intent.incarnation.to_string(),
            "--deadline-ns".to_owned(),
            stage.to_string(),
        ];
        let mut expected_argv = vec![executable.clone()];
        expected_argv.extend(arguments.clone());
        require(
            self.request
                == json!({"schema":"hermit-grouped-runtime-source-launch-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":stage,
            "unit":unit,"executable":executable,"arguments":expected_argv}),
            "outside source exact maintained argv or fields differ",
        )?;
        let executing: OwnedFd = std::fs::File::open(std::env::current_exe()?)?.into();
        let named: OwnedFd = std::fs::File::open(&executable)?.into();
        require(
            owner::stat(self.rights[1].as_raw_fd())?
                .same_object(&owner::stat(executing.as_raw_fd())?)
                && owner::stat(named.as_raw_fd())?
                    .same_object(&owner::stat(executing.as_raw_fd())?),
            "outside source image is not the actual authenticated executing Hermit",
        )?;
        // Inspection-only opens have no launch/custody authority. They are
        // closed before the retained Child and inherited input are acquired.
        drop((executing, named));
        let directory = CString::new("source-launcher").unwrap();
        if unsafe { libc::mkdirat(receipts.as_raw_fd(), directory.as_ptr(), 0o700) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let raw = unsafe {
            libc::openat(
                receipts.as_raw_fd(),
                directory.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        self.directory = Some(unsafe { OwnedFd::from_raw_fd(raw) });
        if unsafe { libc::fsync(receipts.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let expected = owner::stat(raw)?;
        require(
            expected.mode & libc::S_IFMT == libc::S_IFDIR
                && expected.mode & 0o7777 == 0o700
                && expected.uid == unsafe { libc::getuid() },
            "outside source retained log directory differs",
        )?;
        self.input_alias = Some(common::duplicate(self.rights[0].as_fd())?);
        let arguments = arguments
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>();
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
        let argv = launch.arguments_with_grouped_files(
            crate::network_runtime::capability_unit::GroupedOpenFiles::SourceControls,
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
            .stdin(Stdio::from(self.input_alias.take().unwrap()))
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
        self.launch_attempted = true;
        self.launcher = Some(owner::Launcher::retain(command.spawn()?));
        drop(command);
        // The original transfer endpoint must not mask source EOF. Its real
        // Child inherited the endpoint; record and close only this local alias.
        let raw = unsafe { libc::close(self.rights[0].as_raw_fd()) };
        let error = if raw < 0 {
            io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        self.input_close = Some((raw, error));
        if raw < 0 {
            return Err(io::Error::from_raw_os_error(error.unwrap_or(libc::EIO)));
        }
        self.launcher
            .as_mut()
            .unwrap()
            .initialize(self.directory.as_ref().unwrap().as_raw_fd())?;
        // The exact actual Child remains here. This native lease is captured
        // from it; neither a PID nor a remote source record constructs it.
        let lease = self.launcher.as_ref().unwrap().source_lease()?;
        let sampled = Instant::now();
        let now = guardian::monotonic_ns()?;
        common::before(stage)?;
        self.capture = Some(ParentLaunchCustody::retain(
            unit,
            nonce,
            sampled + Duration::from_nanos(stage - now),
            lease,
        ));
        self.capture.as_mut().unwrap().start()?;
        loop {
            common::before(stage)?;
            self.drain()?;
            if self.capture.as_mut().unwrap().progress()? {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.capture_observation = Some(self.capture.as_mut().unwrap().captured_observation()?);
        common::before(stage)
    }
    fn terminal_success(&mut self, intent: &Intent, stage: u64) -> io::Result<bool> {
        common::before(stage)?;
        if self.terminal.is_some() {
            return Ok(true);
        }
        self.drain()?;
        let launcher = self
            .launcher
            .as_mut()
            .ok_or_else(|| io::Error::other("outside actual source Child absent"))?;
        if launcher.eof != [true, true]
            || !owner::terminal(launcher.pidfd.as_ref().unwrap().as_raw_fd())?
        {
            return Ok(false);
        }
        launcher.reap_success(self.directory.as_ref().unwrap().as_raw_fd())?;
        for (index, name) in [c"stdout.log", c"stderr.log"].into_iter().enumerate() {
            let raw = unsafe {
                libc::openat(
                    self.directory.as_ref().unwrap().as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            self.log_readers.push(unsafe { OwnedFd::from_raw_fd(raw) });
            require(
                owner::stat(raw)?.same_object(&owner::stat(
                    launcher.log_files[index].as_ref().unwrap().as_raw_fd(),
                )?),
                "outside source readonly log is not its actual original output",
            )?;
        }
        self.terminal = Some(json!({"schema":"hermit-grouped-runtime-source-terminal-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":stage,"unit":self.request["unit"],
            "wrapper_pid":launcher.child.id(),"raw_wait_status":launcher.reaped.unwrap().into_raw(),
            "eof":launcher.eof,"logs_synced":launcher.logs_synced,"group_absent":true,"global_ECHILD_claimed":true,
            "stdout":{"bytes":launcher.stdout.len(),"sha256":hex(&Sha256::digest(&launcher.stdout))},
            "stderr":{"bytes":launcher.stderr.len(),"sha256":hex(&Sha256::digest(&launcher.stderr))}}));
        common::before(stage)?;
        Ok(true)
    }
}

struct SourceOwner {
    input: wire::Channel,
    run: [u8; 16],
    stage: u64,
    intent: Option<Intent>,
    parent: Option<wire::Credentials>,
    bootstrap: Vec<(Value, Vec<OwnedFd>)>,
    receipts: Option<OwnedFd>,
    parent_pin: Option<OwnedFd>,
    image: Option<OwnedFd>,
    source_unit: Option<String>,
    channel: Option<wire::Channel>,
    export: Option<OwnedFd>,
    keeper: Option<(wire::Credentials, OwnedFd)>,
    controller: Option<(wire::Credentials, OwnedFd)>,
    source: Option<Box<OwnedSource>>,
    ledger: Option<journal::RemovalJournal>,
    ledger_directory: Option<i32>,
    census: owner::CensusInventory,
    closes: Vec<Value>,
    failure: Option<Failure>,
    origin: Option<u64>,
    cutoff: Option<u64>,
    terminal_ack: bool,
    retirement: Option<Value>,
    recovery_transport_error: Option<String>,
    retirement_started: bool,
    receipt_files: Vec<(String, OwnedFd)>,
    receipt_attempts: Vec<Value>,
    capability_setup: Value,
}
impl SourceOwner {
    fn retain(input: OwnedFd, run: [u8; 16], stage: u64) -> Self {
        Self {
            input: wire::Channel::retain(input),
            run,
            stage,
            intent: None,
            parent: None,
            bootstrap: Vec::new(),
            receipts: None,
            parent_pin: None,
            image: None,
            source_unit: None,
            channel: None,
            export: None,
            keeper: None,
            controller: None,
            source: None,
            ledger: None,
            ledger_directory: None,
            census: owner::CensusInventory::retain(),
            closes: Vec::new(),
            failure: None,
            origin: None,
            cutoff: None,
            terminal_ack: false,
            retirement: None,
            recovery_transport_error: None,
            retirement_started: false,
            receipt_files: Vec::new(),
            receipt_attempts: Vec::new(),
            capability_setup: Value::Null,
        }
    }
    fn clear_initial_capabilities(&mut self) -> io::Result<()> {
        // Drop this dedicated helper's inherited/permitted/effective/ambient
        // sets, preserving its bounding set and NNP0 for audited sudo.
        fn fields() -> io::Result<std::collections::BTreeMap<String, String>> {
            let text = owner::read_file("/proc/self/status", 16384)?;
            let keys = [
                "CapInh",
                "CapPrm",
                "CapEff",
                "CapBnd",
                "CapAmb",
                "NoNewPrivs",
            ];
            let mut result = std::collections::BTreeMap::new();
            for line in text.lines() {
                let (key, value) = line
                    .split_once(':')
                    .ok_or_else(|| io::Error::other("source owner capability status malformed"))?;
                if keys.contains(&key) {
                    require(
                        result
                            .insert(key.to_owned(), value.trim().to_owned())
                            .is_none(),
                        "source owner duplicate capability field",
                    )?;
                }
            }
            require(
                result.len() == keys.len(),
                "source owner capability status incomplete",
            )?;
            Ok(result)
        }
        common::before(self.stage)?;
        let before = fields()?;
        self.capability_setup =
            json!({"before":before,"ambient_clear":null,"capset":null,"after":null});
        require(
            before["NoNewPrivs"] == "0"
                && unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } == 0,
            "source owner cannot clear inherited NoNewPrivileges",
        )?;
        let raw = unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL,
                0,
                0,
                0,
            )
        };
        let errno = if raw == -1 {
            io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        self.capability_setup["ambient_clear"] = json!({"attempted":true,"raw":raw,"errno":errno});
        if raw == -1 {
            return Err(io::Error::from_raw_os_error(errno.unwrap_or(libc::EIO)));
        }
        require(
            raw == 0,
            "source owner ambient clear returned unexpected result",
        )?;
        #[repr(C)]
        struct Header {
            version: u32,
            pid: i32,
        }
        #[repr(C)]
        struct Data {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        // Linux capability ABI v3 contains exactly two 32-bit words per set.
        let header = Header {
            version: 0x2008_0522,
            pid: 0,
        };
        let data = [
            Data {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            },
            Data {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            },
        ];
        let raw =
            unsafe { libc::syscall(libc::SYS_capset, &header as *const Header, data.as_ptr()) };
        let errno = if raw == -1 {
            io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        self.capability_setup["capset"] =
            json!({"attempted":true,"raw":raw,"errno":errno,"version":header.version});
        if raw == -1 {
            return Err(io::Error::from_raw_os_error(errno.unwrap_or(libc::EIO)));
        }
        require(raw == 0, "source owner capset returned unexpected result")?;
        let after = fields()?;
        self.capability_setup["after"] = json!(after);
        require(
            after["CapBnd"] == before["CapBnd"]
                && after["NoNewPrivs"] == before["NoNewPrivs"]
                && unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } == 0,
            "source owner capability setup changed bounding set or NNP",
        )?;
        common::before(self.stage)
    }
    fn write_receipt(&mut self, name: &str, bytes: &[u8], cutoff: u64) -> io::Result<()> {
        common::before(cutoff)?;
        require(
            bytes.len() <= 1_048_576
                && !name.contains('/')
                && !name.is_empty()
                && !self.receipt_files.iter().any(|(prior, _)| prior == name),
            "source owner receipt bound/name/repetition differs",
        )?;
        let directory = self
            .receipts
            .as_ref()
            .ok_or_else(|| io::Error::other("source owner original receipt directory absent"))?
            .as_raw_fd();
        let filename = CString::new(name).map_err(io::Error::other)?;
        self.receipt_attempts.push(
            json!({"name":name,"bytes":bytes.len(),"sha256":hex(&Sha256::digest(bytes)),
            "open":null,"writes":[],"file_sync":null,"directory_sync":null,"complete":false}),
        );
        let index = self.receipt_attempts.len() - 1;
        let raw = unsafe {
            libc::openat(
                directory,
                filename.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        let error = if raw < 0 {
            io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        // Install the actual returned description before any fallible write or
        // validation; failure retains it in the exact final ownership census.
        if raw >= 0 {
            self.receipt_files
                .push((name.to_owned(), unsafe { OwnedFd::from_raw_fd(raw) }));
        }
        self.receipt_attempts[index]["open"] = json!({"raw":raw,"errno":error});
        if raw < 0 {
            return Err(io::Error::from_raw_os_error(error.unwrap_or(libc::EIO)));
        }
        let mut offset = 0;
        while offset < bytes.len() {
            common::before(cutoff)?;
            let count =
                unsafe { libc::write(raw, bytes[offset..].as_ptr().cast(), bytes.len() - offset) };
            let error = if count < 0 {
                io::Error::last_os_error().raw_os_error()
            } else {
                None
            };
            self.receipt_attempts[index]["writes"]
                .as_array_mut()
                .unwrap()
                .push(json!({"offset":offset,"raw":count,"errno":error}));
            if count < 0 {
                return Err(io::Error::from_raw_os_error(error.unwrap_or(libc::EIO)));
            }
            require(count > 0, "source owner receipt write made no progress")?;
            offset += count as usize;
        }
        for (key, fd) in [("file_sync", raw), ("directory_sync", directory)] {
            common::before(cutoff)?;
            let result = unsafe { libc::fsync(fd) };
            let error = if result < 0 {
                io::Error::last_os_error().raw_os_error()
            } else {
                None
            };
            self.receipt_attempts[index][key] = json!({"raw":result,"errno":error});
            if result < 0 {
                return Err(io::Error::from_raw_os_error(error.unwrap_or(libc::EIO)));
            }
            require(
                result == 0,
                "source owner receipt fsync returned unexpected result",
            )?;
        }
        common::before(cutoff)?;
        self.receipt_attempts[index]["complete"] = json!(true);
        Ok(())
    }
    fn persist_first_failure(&mut self, error: &io::Error) -> io::Result<()> {
        let cutoff = self
            .cutoff
            .ok_or_else(|| io::Error::other("source first failure cutoff absent"))?;
        let source = self.source.as_ref();
        let observation = json!({"schema":"hermit-grouped-source-owner-first-failure-v1","run":hex(&self.run),
            "stage_deadline":self.stage,"first_failure_origin":self.origin,"cutoff":cutoff,
            "primary":{"message":error.to_string(),"kind":format!("{:?}",error.kind()),"errno":error.raw_os_error()},
            "capability_setup":self.capability_setup,
            "source":source.map(|source|json!({"request":source.request,"launch_attempted":source.launch_attempted,
                "input_close":source.input_close,"capture":source.capture.as_ref().map(ParentLaunchCustody::diagnostics),
                "capture_observation":source.capture_observation,"terminal":source.terminal,"sent_terminal":source.sent_terminal,
                "launcher":source.launcher.as_ref().map(|launcher|json!({"pid":launcher.child.id(),
                    "pidfd_held":launcher.pidfd.is_some(),"raw_wait_status":launcher.reaped.map(ExitStatusExt::into_raw),
                    "eof":launcher.eof,"logs_synced":launcher.logs_synced,
                    "stdout":{"bytes":launcher.stdout.len(),"sha256":hex(&Sha256::digest(&launcher.stdout))},
                    "stderr":{"bytes":launcher.stderr.len(),"sha256":hex(&Sha256::digest(&launcher.stderr))}}))})),
            "prior_receipt_attempts":self.receipt_attempts,
            "owner_ledger_refused":self.ledger.as_ref().and_then(|ledger|ledger.store.refused.as_ref()).map(|failure|failure.error().to_string())});
        let bytes = journal::canonical(&observation)?;
        self.write_receipt("source-owner-first-failure.json", &bytes, cutoff)?;
        // The original journal may itself be the refused operation. Never reset
        // it or replace that cause: the independently fsynced file remains the
        // diagnostic authority, and only a healthy Store gets its hash row.
        if let Some(ledger) = &mut self.ledger
            && ledger.store.refused.is_none()
        {
                ledger.store.append(json!({"kind":"source-owner-first-failure",
                "file":"source-owner-first-failure.json","bytes":bytes.len(),"sha256":hex(&Sha256::digest(&bytes))}))?;
            }
        common::before(cutoff)
    }
    fn observe(&mut self, cutoff: u64) -> io::Result<()> {
        common::before(cutoff)?;
        let now = guardian::monotonic_ns()?;
        self.census.observe(
            &[self.input.fd.as_raw_fd()],
            Instant::now() + Duration::from_nanos(cutoff - now),
        )?;
        common::before(cutoff)
    }
    fn close(&mut self, fd: i32, cutoff: u64) -> io::Result<()> {
        common::before(cutoff)?;
        // SCM receipt may legitimately reuse an earlier closed numeric slot.
        // Bind each actual close to the currently held description; the terminal
        // owner set and one-use retirement gate prevent a repeated alias close.
        let description = owner::describe_fd(fd)?;
        self.closes.push(
            json!({"fd":fd,"description":description,"attempted":false,"raw":null,"errno":null}),
        );
        let row = self.closes.last_mut().unwrap();
        row["attempted"] = json!(true);
        let raw = unsafe { libc::close(fd) };
        let error = if raw < 0 {
            io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        row["raw"] = json!(raw);
        row["errno"] = json!(error);
        require(raw == 0, "source owner explicit alias close failed")?;
        common::before(cutoff)
    }
    fn bootstrap(&mut self) -> io::Result<()> {
        self.input.validate()?;
        let mut cred = unsafe { std::mem::zeroed::<libc::ucred>() };
        let mut size = std::mem::size_of_val(&cred) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                self.input.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let parent = common::credentials(cred.pid)?;
        require(
            size as usize == std::mem::size_of_val(&cred)
                && cred.uid == parent.uid
                && cred.gid == parent.gid,
            "source owner original parent credentials differ",
        )?;
        self.parent = Some(parent);
        let index = common::receive(&mut self.input, parent, 3, 4096, self.stage)?;
        let packet = &mut self.input.packets[index];
        self.bootstrap
            .push((Value::Null, std::mem::take(&mut packet.rights)));
        self.bootstrap[0].0 = serde_json::from_slice(&packet.bytes)?;
        let header = &self.bootstrap[0].0;
        let unit = header["source_unit"]
            .as_str()
            .ok_or_else(|| io::Error::other("source owner exact unit absent"))?
            .to_owned();
        let unit_nonce = unit
            .strip_prefix("hermit-accepted-")
            .and_then(|s| s.strip_suffix(".service"))
            .ok_or_else(|| io::Error::other("source owner exact unit malformed"))?;
        require(
            super::valid_nonce(unit_nonce)
                && *header
                    == json!({"schema":"hermit-grouped-source-owner-bootstrap-v1",
            "nonce":hex(&self.run),"incarnation":u64::from_le_bytes(self.run[..8].try_into().unwrap()),
            "stage_deadline":self.stage,"source_unit":unit}),
            "source owner original bootstrap differs",
        )?;
        self.source_unit = Some(unit);
        self.intent = Some(Intent::new(
            hex(&self.run),
            u64::from_le_bytes(self.run[..8].try_into().unwrap()),
        )?);
        let mut rights = std::mem::take(&mut self.bootstrap[0].1).into_iter();
        self.receipts = rights.next();
        self.parent_pin = rights.next();
        self.image = rights.next();
        owner::pidfd_matches(self.parent_pin.as_ref().unwrap().as_raw_fd(), parent.pid)?;
        require(
            !owner::terminal(self.parent_pin.as_ref().unwrap().as_raw_fd())?,
            "source owner parent already terminal",
        )?;
        let stat = owner::stat(self.receipts.as_ref().unwrap().as_raw_fd())?;
        require(
            stat.mode & libc::S_IFMT == libc::S_IFDIR
                && stat.mode & 0o7777 == 0o700
                && stat.uid == parent.uid,
            "source owner original receipt directory differs",
        )?;
        let executing: OwnedFd = std::fs::File::open(std::env::current_exe()?)?.into();
        require(
            owner::stat(executing.as_raw_fd())?
                .same_object(&owner::stat(self.image.as_ref().unwrap().as_raw_fd())?),
            "source owner held image differs from executing image",
        )?;
        drop(executing);
        let directory = common::duplicate(self.receipts.as_ref().unwrap().as_fd())?;
        self.ledger_directory = Some(directory.as_raw_fd());
        self.ledger = Some(journal::RemovalJournal::retain(
            directory,
            self.intent.as_ref().unwrap().clone(),
        ));
        self.ledger
            .as_mut()
            .unwrap()
            .initialize("ordinary-source-owner")?;
        let mut pair = [-1; 2];
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        self.channel = Some(wire::Channel::retain(unsafe {
            OwnedFd::from_raw_fd(pair[0])
        }));
        self.export = Some(unsafe { OwnedFd::from_raw_fd(pair[1]) });
        let enabled = 1i32;
        for fd in pair {
            if unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_PASSCRED,
                    (&enabled as *const i32).cast(),
                    std::mem::size_of_val(&enabled) as libc::socklen_t,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        common::send(
            &mut self.input,
            &json!({"schema":"hermit-grouped-source-owner-channel-v1","nonce":hex(&self.run),
            "incarnation":self.intent.as_ref().unwrap().incarnation,"stage_deadline":self.stage}),
            &[self.export.as_ref().unwrap().as_fd()],
            self.stage,
        )?;
        self.close(self.export.as_ref().unwrap().as_raw_fd(), self.stage)?;
        for (sequence, role) in [(1, "keeper"), (2, "startup")] {
            let index = common::receive(&mut self.input, parent, 1, 4096, self.stage)?;
            let packet = &mut self.input.packets[index];
            self.bootstrap
                .push((Value::Null, std::mem::take(&mut packet.rights)));
            let frame = self.bootstrap.last_mut().unwrap();
            frame.0 = serde_json::from_slice(&packet.bytes)?;
            let pid = frame.0["pid"]
                .as_i64()
                .and_then(|v| i32::try_from(v).ok())
                .ok_or_else(|| io::Error::other("source owner linked PID absent"))?;
            require(
                frame.0
                    == json!({"schema":"hermit-grouped-source-owner-link-v1","nonce":hex(&self.run),
                "incarnation":self.intent.as_ref().unwrap().incarnation,"stage_deadline":self.stage,
                "sequence":sequence,"role":role,"pid":pid}),
                "source owner linked role changed",
            )?;
            let pin = frame.1.pop().unwrap();
            let credentials = common::credentials(pid)?;
            if sequence == 1 {
                self.keeper = Some((credentials, pin));
            } else {
                self.controller = Some((credentials, pin));
            }
            let held = if sequence == 1 {
                self.keeper.as_ref().unwrap()
            } else {
                self.controller.as_ref().unwrap()
            };
            owner::pidfd_matches(held.1.as_raw_fd(), held.0.pid)?;
            require(
                !owner::terminal(held.1.as_raw_fd())?,
                "source owner linked original peer terminal",
            )?;
        }
        self.observe(self.stage)?;
        owner::check_no_children()?;
        common::send(
            &mut self.input,
            &json!({"schema":"hermit-grouped-source-owner-ready-v1","nonce":hex(&self.run),
            "incarnation":self.intent.as_ref().unwrap().incarnation,"stage_deadline":self.stage}),
            &[],
            self.stage,
        )
    }
    fn launch(&mut self) -> io::Result<()> {
        let peer = self.keeper.as_ref().unwrap().0;
        let index = common::receive(self.channel.as_mut().unwrap(), peer, 2, 4096, self.stage)?;
        let packet = &mut self.channel.as_mut().unwrap().packets[index];
        self.source = Some(Box::new(OwnedSource::retain(
            Value::Null,
            std::mem::take(&mut packet.rights),
        )));
        self.source.as_mut().unwrap().request = serde_json::from_slice(&packet.bytes)?;
        require(
            self.source.as_ref().unwrap().request["unit"]
                == self.source_unit.as_ref().unwrap().as_str(),
            "source owner request substituted original unit",
        )?;
        // Preserve OwnedSource's executing-image object identity contract.
        // The parent and controller hold separate opens of that same image;
        // a failed OFD comparison is never reinterpreted as this proof.
        require(
            owner::stat(self.source.as_ref().unwrap().rights[1].as_raw_fd())?
                .same_object(&owner::stat(self.image.as_ref().unwrap().as_raw_fd())?),
            "source launch image differs",
        )?;
        self.source.as_mut().unwrap().retirement = Some(owner::OutsideSourceRetirement::retain(
            common::duplicate(self.controller.as_ref().unwrap().1.as_fd())?,
            self.stage,
        ));
        self.source.as_mut().unwrap().spawn(
            self.intent.as_ref().unwrap(),
            self.stage,
            self.receipts.as_ref().unwrap().as_fd(),
        )?;
        let source = self.source.as_ref().unwrap();
        let capture_bytes = journal::canonical(&json!({"request":source.request,
            "capture":source.capture_observation,"input_close":source.input_close}))?;
        // Keep the unchanged 4096-byte Store row bound: full native capture is
        // retained in its bounded file, as in the original Keeper path.
        self.write_receipt(
            "source-launch-native-capture.json",
            &capture_bytes,
            self.stage,
        )?;
        self.ledger.as_mut().unwrap().store.append(
            json!({"kind":"original-source-launch-and-capture",
            "file":"source-launch-native-capture.json","bytes":capture_bytes.len(),
            "sha256":hex(&Sha256::digest(&capture_bytes))}),
        )?;
        let source = self.source.as_ref().unwrap();
        let capture = source.capture.as_ref().unwrap().captured_native_inputs()?;
        let creator = json!({"unit":capture.unit,"pid":capture.pid,"invocation":capture.invocation,"cgroup":capture.cgroup,
            "device":capture.directory_identity.device,"inode":capture.directory_identity.inode,
            "creator_pidfd_held":true,"cgroup_directory_held":true});
        let launcher = source.launcher.as_ref().unwrap();
        common::send(
            self.channel.as_mut().unwrap(),
            &json!({"schema":"hermit-grouped-source-owner-launched-v1",
            "nonce":hex(&self.run),"incarnation":self.intent.as_ref().unwrap().incarnation,"stage_deadline":self.stage,
            "unit":self.source_unit,"wrapper_pid":launcher.child.id(),"wait_owner_pid":unsafe{libc::getpid()},"creator":creator}),
            &[
                launcher.pidfd.as_ref().unwrap().as_fd(),
                capture.pidfd,
                capture.directory,
            ],
            self.stage,
        )
    }
    fn remember(
        &mut self,
        error: &io::Error,
        origin: Option<u64>,
        cutoff: Option<u64>,
    ) -> io::Result<()> {
        self.failure.get_or_insert_with(|| Failure::capture(error));
        let now = guardian::monotonic_ns()?;
        let origin = origin.unwrap_or(now);
        require(
            origin > 0 && origin <= now,
            "source owner failure origin future or zero",
        )?;
        self.origin = Some(self.origin.map_or(origin, |old| old.min(origin)));
        let bounded = self
            .origin
            .unwrap()
            .checked_add(1_000_000_000)
            .ok_or_else(|| io::Error::other("source owner failure bound overflow"))?
            .min(self.stage)
            .min(cutoff.unwrap_or(self.stage));
        self.cutoff = Some(self.cutoff.map_or(bounded, |old| old.min(bounded)));
        Ok(())
    }
    fn drive(&mut self) -> io::Result<()> {
        loop {
            common::before(self.stage)?;
            if let Some(index) = self.channel.as_mut().unwrap().receive(4096)? {
                let packet = &self.channel.as_ref().unwrap().packets[index];
                packet.exact(0, self.keeper.as_ref().unwrap().0)?;
                let value: Value = serde_json::from_slice(&packet.bytes)?;
                require(
                    journal::canonical(&value)? == packet.bytes,
                    "source owner request is not canonical",
                )?;
                if value["schema"] == "hermit-grouped-source-owner-retire-v1" {
                    let origin = value["original_start"]
                        .as_u64()
                        .ok_or_else(|| io::Error::other("source owner original failure absent"))?;
                    let cutoff = value["cutoff"]
                        .as_u64()
                        .ok_or_else(|| io::Error::other("source owner original cutoff absent"))?;
                    require(
                        value
                            == json!({"schema":"hermit-grouped-source-owner-retire-v1","nonce":hex(&self.run),
                        "incarnation":self.intent.as_ref().unwrap().incarnation,"stage_deadline":self.stage,
                        "original_start":origin,"cutoff":cutoff}),
                        "source owner retirement identity changed",
                    )?;
                    let error = io::Error::other("original Keeper requested source retirement");
                    self.remember(&error, Some(origin), Some(cutoff))?;
                    return Err(error);
                }
                let source = self.source.as_ref().unwrap();
                require(
                    source.sent_terminal
                        && value
                            == json!({"schema":"hermit-grouped-source-owner-terminal-ack-v1",
                    "nonce":hex(&self.run),"incarnation":self.intent.as_ref().unwrap().incarnation,"stage_deadline":self.stage,
                    "wrapper_pid":source.launcher.as_ref().unwrap().child.id()}),
                    "source owner terminal ACK precedes admitted completed archive",
                )?;
                self.terminal_ack = true;
                owner::check_no_children()?;
                return Ok(());
            }
            if owner::terminal(self.controller.as_ref().unwrap().1.as_raw_fd())?
                || owner::terminal(self.keeper.as_ref().unwrap().1.as_raw_fd())?
            {
                return Err(io::Error::other(
                    "source owner original startup or Keeper became terminal before completed handoff",
                ));
            }
            let source = self.source.as_mut().unwrap();
            if !source.sent_terminal
                && source.terminal_success(self.intent.as_ref().unwrap(), self.stage)?
            {
                source.sent_terminal = true;
                common::send(
                    self.channel.as_mut().unwrap(),
                    source.terminal.as_ref().unwrap(),
                    &[source.log_readers[0].as_fd(), source.log_readers[1].as_fd()],
                    self.stage,
                )?;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn recover(&mut self) -> io::Result<()> {
        let mut cutoff = self
            .cutoff
            .ok_or_else(|| io::Error::other("source owner first failure cutoff absent"))?;
        loop {
            common::before(cutoff)?;
            // Losing the Keeper channel cannot remove this owner's actual
            // controller pin/Child/manager retirement authority. Retain transport
            // failure separately; only a fully authenticated earlier cause may
            // tighten the original native retirement bound.
            let keeper_live = match self.keeper.as_ref() {
                Some((_, pin)) => match owner::terminal(pin.as_raw_fd()) {
                    Ok(terminal) => !terminal,
                    Err(error) => {
                        self.recovery_transport_error = Some(error.to_string());
                        false
                    }
                },
                None => false,
            };
            if self.recovery_transport_error.is_none() && keeper_live {
                let received = (|| -> io::Result<Option<(u64, u64)>> {
                    let Some(channel) = &mut self.channel else {
                        return Ok(None);
                    };
                    let peer = self.keeper.as_ref().unwrap().0;
                    let Some(index) = channel.receive(4096)? else {
                        return Ok(None);
                    };
                    let packet = &channel.packets[index];
                    packet.exact(0, peer)?;
                    let value: Value = serde_json::from_slice(&packet.bytes)?;
                    let origin = value["original_start"]
                        .as_u64()
                        .ok_or_else(|| io::Error::other("source original failure absent"))?;
                    let enclosing = value["cutoff"]
                        .as_u64()
                        .ok_or_else(|| io::Error::other("source original cutoff absent"))?;
                    require(
                        packet.bytes == journal::canonical(&value)?
                            && value
                                == json!({"schema":"hermit-grouped-source-owner-retire-v1",
                        "nonce":hex(&self.run),"incarnation":self.intent.as_ref().unwrap().incarnation,"stage_deadline":self.stage,
                        "original_start":origin,"cutoff":enclosing}),
                        "source retirement origin or identity changed",
                    )?;
                    Ok(Some((origin, enclosing)))
                })();
                match received {
                    Ok(Some((origin, enclosing))) => {
                        self.remember(
                            &io::Error::other("original Keeper source retirement"),
                            Some(origin),
                            Some(enclosing),
                        )?;
                        cutoff = self.cutoff.unwrap();
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => self.recovery_transport_error = Some(error.to_string()),
                }
            }
            if owner::terminal(
                self.controller
                    .as_ref()
                    .ok_or_else(|| io::Error::other("source original controller absent"))?
                    .1
                    .as_raw_fd(),
            )? {
                break;
            }
            if let Some(source) = &mut self.source {
                source.drain()?;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let source = self
            .source
            .as_mut()
            .ok_or_else(|| io::Error::other("source actual Child was never acquired"))?;
        let capture = source
            .capture
            .as_ref()
            .ok_or_else(|| io::Error::other("source original manager capture absent"))?;
        let launcher = source
            .launcher
            .as_mut()
            .ok_or_else(|| io::Error::other("source actual original Launcher absent"))?;
        let retirement = source.retirement.as_mut().unwrap();
        loop {
            common::before(cutoff)?;
            if retirement.progress(
                capture,
                launcher,
                source.directory.as_ref().unwrap().as_fd(),
                &mut self.ledger.as_mut().unwrap().store,
                self.origin.unwrap(),
                cutoff,
            )? {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        require(
            launcher.reaped.is_some() && launcher.eof == [true, true] && launcher.logs_synced,
            "source owner actual terminal wait/EOF/logs incomplete",
        )?;
        owner::check_no_children()?;
        self.retirement = Some(
            json!({"schema":"hermit-grouped-source-owner-retired-v1","nonce":hex(&self.run),
            "incarnation":self.intent.as_ref().unwrap().incarnation,"stage_deadline":self.stage,
            "unit":self.source_unit,"wrapper_pid":launcher.child.id(),"raw_wait_status":launcher.reaped.unwrap().into_raw(),
            "eof":launcher.eof,"logs_synced":launcher.logs_synced,"group_absent":true,"global_ECHILD_claimed":true,
            "original_start":self.origin,"cutoff":cutoff}),
        );
        self.ledger
            .as_mut()
            .unwrap()
            .store
            .append(json!({"kind":"original-source-retirement",
            "result":self.retirement,"native":retirement.diagnostics()}))?;
        if !owner::terminal(self.keeper.as_ref().unwrap().1.as_raw_fd())? {
            common::send(
                self.channel.as_mut().unwrap(),
                self.retirement.as_ref().unwrap(),
                &[],
                cutoff,
            )?;
        }
        self.close_owned(cutoff)
    }
    fn close_owned(&mut self, cutoff: u64) -> io::Result<()> {
        require(
            !self.retirement_started,
            "source owner terminal retirement cannot repeat",
        )?;
        owner::check_no_children()?;
        self.observe(cutoff)?;
        let mut owned = vec![self.input.fd.as_raw_fd(), self.census.held_descriptor()?];
        for fd in [&self.receipts, &self.parent_pin, &self.image]
            .into_iter()
            .flatten()
        {
            owned.push(fd.as_raw_fd());
        }
        if let Some(link) = &self.keeper {
            owned.push(link.1.as_raw_fd());
        }
        if let Some(link) = &self.controller {
            owned.push(link.1.as_raw_fd());
        }
        if let Some(channel) = &self.channel {
            owned.push(channel.fd.as_raw_fd());
            for packet in &channel.packets {
                owned.extend(packet.rights.iter().map(AsRawFd::as_raw_fd));
            }
        }
        for packet in &self.input.packets {
            owned.extend(packet.rights.iter().map(AsRawFd::as_raw_fd));
        }
        for (_, rights) in &self.bootstrap {
            owned.extend(rights.iter().map(AsRawFd::as_raw_fd));
        }
        if let Some(fd) = self.ledger_directory {
            owned.push(fd);
        }
        if let Some(ledger) = &self.ledger
            && let Some(fd) = &ledger.store.file
        {
                owned.push(fd.as_raw_fd());
            }
        if let Some(source) = &self.source {
            owned.extend(
                source
                    .rights
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != 0 || source.input_close != Some((0, None)))
                    .map(|(_, fd)| fd.as_raw_fd()),
            );
            for fd in [&source.directory, &source.input_alias].into_iter().flatten() {
                owned.push(fd.as_raw_fd());
            }
            if let Some(launcher) = &source.launcher {
                if let Some(fd) = &launcher.pidfd {
                    owned.push(fd.as_raw_fd());
                }
                if let Some(fd) = &launcher.child.stdout {
                    owned.push(fd.as_raw_fd());
                }
                if let Some(fd) = &launcher.child.stderr {
                    owned.push(fd.as_raw_fd());
                }
                owned.extend(launcher.log_files.iter().flatten().map(AsRawFd::as_raw_fd));
            }
            owned.extend(source.log_readers.iter().map(AsRawFd::as_raw_fd));
            if let Some(capture) = &source.capture {
                owned.extend(capture.held_descriptors());
            }
            if let Some(retirement) = &source.retirement {
                owned.extend(retirement.held_descriptors());
            }
        }
        owned.extend(self.receipt_files.iter().map(|(_, fd)| fd.as_raw_fd()));
        owned.sort_unstable();
        require(
            owned.len() <= 125
                && owned.iter().all(|fd| *fd > 2)
                && owned.windows(2).all(|p| p[0] != p[1]),
            "source owner retained descriptor population malformed",
        )?;
        let mut expected = vec![0, 1, 2];
        expected.extend(&owned);
        expected.sort_unstable();
        require(
            self.census.last_descriptors()? == expected,
            "source owner census contains an unowned descriptor",
        )?;
        self.retirement_started = true;
        for fd in owned {
            self.close(fd, cutoff)?;
        }
        owner::check_no_children()?;
        common::before(cutoff)
    }
    fn run(&mut self) -> io::Result<()> {
        self.bootstrap()?;
        self.launch()?;
        self.drive()?;
        require(self.terminal_ack, "source owner normal handoff absent")?;
        self.close_owned(self.stage)
    }
}

/// Run the fixed ordinary source owner in its separately captured user unit.
///
/// # Safety
/// The caller must be the dedicated early process entry and transfer its real
/// original stdin endpoint before any parsing or fallible initialization.
pub unsafe fn run_grouped_source_owner_process(input: OwnedFd, run: [u8; 16], stage: u64) -> ! {
    let mut state = ManuallyDrop::new(SourceOwner::retain(input, run, stage));
    let result = (|| {
        let now = guardian::monotonic_ns()?;
        require(
            run != [0; 16] && stage > now && stage - now <= 20_000_000_000,
            "source owner original stage bound differs",
        )?;
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0
            || unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        owner::protected_holder()?;
        state.clear_initial_capabilities()?;
        owner::check_source_authority_policy(
            unsafe { libc::getpid() },
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
        )?;
        owner::check_no_children()?;
        state.run()
    })();
    let mut first_failure_persist_error = None;
    let recovery = if let Err(error) = &result {
        let remembered = state.remember(error, None, None);
        // Persist the actual first failure before recovery mutates custody or a
        // closed parent output pipe can hide the final stderr diagnostic.
        first_failure_persist_error = state
            .persist_first_failure(error)
            .err()
            .map(|e| e.to_string());
        remembered
            .and_then(|()| state.recover())
            .err()
            .map(|e| e.to_string())
    } else {
        None
    };
    eprintln!(
        "{}",
        json!({"schema":"hermit-grouped-source-owner-result-v1","run":hex(&run),
        "success":result.is_ok(),"error":result.as_ref().err().map(ToString::to_string),
        "first_failure_origin":state.origin,"cutoff":state.cutoff,"recovery_error":recovery,
        "capability_setup":state.capability_setup,"first_failure_persist_error":first_failure_persist_error,
        "receipt_attempts":state.receipt_attempts,
        "terminal_ack":state.terminal_ack,"retirement":state.retirement,"recovery_transport_error":state.recovery_transport_error,"closes":state.closes})
    );
    unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) }
}
