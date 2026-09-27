//! The ordinary, separately owned user-manager source launcher. This role has
//! no ambient capability and requires NNP=0 for the existing audited sudo path.
//! It is not a Creator and never changes capability-service authentication.
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;

use super::super::guardian;
use super::super::journal;
use super::super::wire;
use super::*;

/// Actual policy of the ordinary source-owner entry, distinct from NNP1
/// capability-bearing source/provider/RuntimeKeeper policy.
pub(in super::super) fn check_source_authority_policy(
    pid: i32,
    uid: u32,
    gid: u32,
) -> io::Result<()> {
    let status = read_file(&format!("/proc/{pid}/status"), 16384)?;
    let keys = [
        "Pid",
        "Tgid",
        "Uid",
        "Gid",
        "TracerPid",
        "Threads",
        "NoNewPrivs",
        "CapInh",
        "CapPrm",
        "CapEff",
        "CapAmb",
    ];
    let mut fields = BTreeMap::new();
    for line in status.lines() {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::other("source authority status malformed"))?;
        if keys.contains(&key) {
            require(
                fields.insert(key, value.trim()).is_none(),
                "source authority duplicate policy field",
            )?;
        }
    }
    require(
        fields.len() == keys.len()
            && fields["Pid"] == pid.to_string()
            && fields["Tgid"] == pid.to_string()
            && fields["TracerPid"] == "0"
            && fields["Threads"] == "1"
            && fields["NoNewPrivs"] == "0",
        "ordinary source authority PID/thread/tracer/NNP policy differs",
    )?;
    for (key, id) in [("Uid", uid), ("Gid", gid)] {
        let values: Vec<_> = fields[key].split_whitespace().collect();
        require(
            values.len() == 4 && values.iter().all(|s| *s == id.to_string()),
            "ordinary source authority credentials differ",
        )?;
    }
    for key in ["CapInh", "CapPrm", "CapEff", "CapAmb"] {
        require(
            fields[key] == "0000000000000000",
            "ordinary source authority has added capabilities",
        )?;
    }
    for (resource, expected) in [
        (libc::RLIMIT_NOFILE, 128),
        (libc::RLIMIT_FSIZE, 1048576),
        (libc::RLIMIT_CORE, 0),
    ] {
        let mut value = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        if unsafe { libc::prlimit(pid, resource, std::ptr::null(), value.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let value = unsafe { value.assume_init() };
        require(
            value.rlim_cur == expected && value.rlim_max == expected,
            "ordinary source authority process bound differs",
        )?;
    }
    if pid == unsafe { libc::getpid() } {
        require(
            unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } == 0,
            "source authority native NNP differs",
        )?;
        protected_holder()?;
        let text = read_file("/proc/self/cgroup", 4096)?;
        let group = text
            .strip_prefix("0::")
            .and_then(|s| s.strip_suffix('\n'))
            .ok_or_else(|| io::Error::other("source authority actual entry cgroup malformed"))?;
        let unit = group.rsplit('/').next().unwrap_or("");
        require(
            group.starts_with(&format!("/user.slice/user-{uid}.slice/user@{uid}.service/"))
                && !group.split('/').any(|s| matches!(s, "." | ".."))
                && unit
                    .strip_prefix("hermit-source-owner-")
                    .and_then(|s| s.strip_suffix(".service"))
                    .is_some_and(super::super::valid_nonce),
            "source authority entry is not its fixed ordinary user unit",
        )?;
        let path = CString::new(format!("/sys/fs/cgroup{group}")).map_err(io::Error::other)?;
        let raw = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let directory = unsafe { OwnedFd::from_raw_fd(raw) };
        require(
            filesystem(directory.as_raw_fd())? == 0x6367_7270,
            "source authority entry directory is not cgroup2",
        )?;
        for (name, value) in [
            ("memory.max", "268435456\n"),
            ("memory.swap.max", "0\n"),
            ("pids.max", "8\n"),
            ("cpu.max", "100000 100000\n"),
        ] {
            require(
                read_at(directory.as_raw_fd(), name)? == value,
                "source authority entry cgroup resource bound differs",
            )?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Identity {
    pid: i32,
    invocation: String,
    cgroup: String,
}

/// Retains the real user-unit wrapper Child, all queries and every partially
/// acquired helper/native handle. No Drop operation means retirement.
pub(in super::super) struct SourceAuthorityUnit {
    unit: String,
    nonce: String,
    helper: PathBuf,
    arguments: Vec<OsString>,
    input: OwnedFd,
    logs: OwnedFd,
    image: EntryImage,
    deadline: Instant,
    stage: u64,
    uid: u32,
    gid: u32,
    launcher: Option<Launcher>,
    queries: Vec<ManagerQuery>,
    snapshots: Vec<ManagerSnapshot>,
    commands: Vec<CommandQuery>,
    entry: Option<EntryQuery>,
    entry_snapshot: Option<EntrySnapshot>,
    starting: Option<Identity>,
    identity: Option<Identity>,
    pidfd: Option<OwnedFd>,
    directory: Option<OwnedFd>,
    directory_identity: Option<FileIdentity>,
    live_snapshots: Vec<usize>,
    terminal_snapshot: Option<usize>,
    started: bool,
    captured: bool,
    terminal_ready: bool,
    stop_attempted: bool,
    reset_attempted: bool,
    retired: bool,
    retirement_cutoff: Option<u64>,
    wait_observation: Option<Value>,
    refusal: Option<Failure>,
    failure_origin: Option<u64>,
}
impl SourceAuthorityUnit {
    pub fn retain(
        unit: String,
        nonce: String,
        helper: PathBuf,
        arguments: Vec<OsString>,
        input: OwnedFd,
        logs: OwnedFd,
        image: OwnedFd,
        deadline: Instant,
        stage: u64,
    ) -> Self {
        let mut argv = vec![helper.as_os_str().to_owned()];
        argv.extend(arguments.iter().cloned());
        Self {
            unit,
            nonce,
            helper,
            arguments,
            input,
            logs,
            image: EntryImage::retain(image, argv),
            deadline,
            stage,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            launcher: None,
            queries: Vec::new(),
            snapshots: Vec::new(),
            commands: Vec::new(),
            entry: None,
            entry_snapshot: None,
            starting: None,
            identity: None,
            pidfd: None,
            directory: None,
            directory_identity: None,
            live_snapshots: Vec::new(),
            terminal_snapshot: None,
            started: false,
            captured: false,
            terminal_ready: false,
            stop_attempted: false,
            reset_attempted: false,
            retired: false,
            retirement_cutoff: None,
            wait_observation: None,
            refusal: None,
            failure_origin: None,
        }
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            if self.refusal.is_none() {
                self.refusal = Some(Failure::capture(error));
                self.failure_origin = guardian::monotonic_ns().ok();
            }
        }
        result
    }
    fn bound(&self, cutoff: u64) -> io::Result<Instant> {
        let sampled = Instant::now();
        let now = guardian::monotonic_ns()?;
        require(
            now < cutoff && cutoff <= self.stage && sampled < self.deadline,
            "source authority original deadline expired",
        )?;
        Ok(self
            .deadline
            .min(sampled + Duration::from_nanos(cutoff - now)))
    }
    fn capture_bound(&self) -> io::Result<Instant> {
        if let Some(error) = &self.refusal {
            return Err(error.error());
        }
        self.bound(self.stage)
    }
    fn count(&self) -> io::Result<()> {
        require(
            self.queries.len() + self.commands.len() + usize::from(self.entry.is_some()) < 64,
            "source authority original64 command bound",
        )
    }
    fn user_command(&self, program: &str) -> io::Result<Command> {
        require(
            self.uid == unsafe { libc::geteuid() } && self.gid == unsafe { libc::getegid() },
            "source authority launcher credentials differ",
        )?;
        let runtime = format!("/run/user/{}", self.uid);
        let path = CString::new(runtime.as_str()).unwrap();
        let mut s = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::lstat(path.as_ptr(), s.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let s = unsafe { s.assume_init() };
        require(
            s.st_mode & libc::S_IFMT == libc::S_IFDIR
                && s.st_mode & 0o7777 == 0o700
                && s.st_uid == self.uid,
            "source authority user-manager runtime directory differs",
        )?;
        let mut command = Command::new(program);
        command
            .env_clear()
            .envs(
                super::super::super::capability_unit::CAPABILITY_ENVIRONMENT
                    .iter()
                    .copied(),
            )
            .env("XDG_RUNTIME_DIR", &runtime)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={runtime}/bus"),
            );
        Ok(command)
    }
    pub fn start(&mut self) -> io::Result<()> {
        let result = (|| {
            self.capture_bound()?;
            require(!self.started, "source authority cannot restart")?;
            self.started = true;
            require(
                super::super::valid_nonce(&self.nonce)
                    && self.unit == format!("hermit-source-owner-{}.service", self.nonce)
                    && self.helper.is_absolute(),
                "source authority fixed unit or executable differs",
            )?;
            require(
                self.arguments.len() == 5
                    && self.arguments[0] == "--grouped-source-owner-private-stdin-v1"
                    && self.arguments[1] == "--run"
                    && self.arguments[2]
                        .to_str()
                        .is_some_and(super::super::valid_nonce)
                    && self.arguments[3] == "--deadline-ns"
                    && self.arguments[4].to_str() == Some(self.stage.to_string().as_str()),
                "source authority fixed private argv differs",
            )?;
            self.image.initialize()?;
            let now = guardian::monotonic_ns()?;
            let remaining = self
                .stage
                .checked_sub(now)
                .filter(|n| *n > 0 && *n <= 20_000_000_000)
                .ok_or_else(|| {
                    io::Error::other("source authority original20s startup bound differs")
                })?;
            let mut command = self.user_command("/usr/bin/systemd-run")?;
            command
                .args([
                    "--user",
                    "--quiet",
                    "--wait",
                    "--pipe",
                    "--expand-environment=no",
                ])
                .arg(format!("--unit={}", self.unit));
            for property in ["Type=exec".to_owned(),"RemainAfterExit=yes".into(),"TimeoutStopSec=1s".into(),
            "KillMode=control-group".into(),"KillSignal=SIGKILL".into(),"SendSIGKILL=yes".into(),
            "MemoryMax=268435456".into(),"MemorySwapMax=0".into(),"TasksMax=8".into(),
            "CPUQuota=100%".into(),"CPUQuotaPeriodSec=100ms".into(),"LimitNOFILE=128".into(),
            "LimitFSIZE=1048576".into(),"LimitCORE=0".into(),"NoNewPrivileges=no".into(),"UMask=0077".into(),
            "Environment=PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C LC_ALL=C".into(),
            "UnsetEnvironment=LD_PRELOAD LD_LIBRARY_PATH LD_AUDIT LD_DEBUG LD_DEBUG_OUTPUT LD_PROFILE LD_PROFILE_OUTPUT LD_BIND_NOT LD_DYNAMIC_WEAK LD_ORIGIN_PATH GLIBC_TUNABLES".into(),
            format!("RuntimeMaxSec={}us",remaining/1000)]{command.arg(format!("--property={property}"));}
            // No User=/capability/sandbox option: this is the original user's
            // manager, and the real helper separately verifies NNP0 and no caps.
            command
                .arg("--")
                .arg(&self.helper)
                .args(&self.arguments)
                .stdin(Stdio::from(self.input.try_clone()?))
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
            self.launcher = Some(Launcher::retain(command.spawn()?));
            self.launcher
                .as_mut()
                .unwrap()
                .initialize(self.logs.as_raw_fd())?;
            self.start_query(self.stage)?;
            Ok(())
        })();
        self.remember(result)
    }
    fn start_query(&mut self, cutoff: u64) -> io::Result<()> {
        self.bound(cutoff)?;
        self.count()?;
        let mut query = ManagerQuery::retain(self.unit.clone());
        query.query.arguments = vec![
            "--user".into(),
            "--no-ask-password".into(),
            "show".into(),
            self.unit.clone(),
            format!("--property={PROPERTIES}"),
        ];
        self.queries.push(query);
        let index = self.queries.len() - 1;
        let mut command = self.user_command("/usr/bin/systemctl")?;
        command
            .args(&self.queries[index].query.arguments)
            .stdin(Stdio::null())
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
        self.queries[index].query.spawn_retained(&mut command)?;
        self.queries[index].query.initialize_child()
    }
    fn poll_query(&mut self, cutoff: u64) -> io::Result<Option<usize>> {
        let deadline = self.bound(cutoff)?;
        let Some(snapshot) = self
            .queries
            .last_mut()
            .ok_or_else(|| io::Error::other("source authority query absent"))?
            .poll(deadline)?
        else {
            return Ok(None);
        };
        self.snapshots.push(snapshot);
        Ok(Some(self.snapshots.len() - 1))
    }
    fn identity(&self, s: &ManagerSnapshot, starting: bool) -> io::Result<Identity> {
        require(
            s.unit() == self.unit
                && s.property("Id") == Some(self.unit.as_str())
                && s.property("LoadState") == Some("loaded")
                && s.property("ActiveState")
                    == Some(if starting { "activating" } else { "active" })
                && s.property("SubState") == Some(if starting { "start" } else { "running" })
                && s.property("ExecMainCode") == Some("0")
                && s.property("ExecMainStatus") == Some("0")
                && s.property("Result") == Some("success"),
            "source authority manager is not original running/start state",
        )?;
        let invocation = s
            .property("InvocationID")
            .filter(|v| super::super::valid_nonce(v))
            .ok_or_else(|| io::Error::other("source authority invocation absent or malformed"))?;
        let text = s
            .property("MainPID")
            .ok_or_else(|| io::Error::other("source authority PID absent"))?;
        let pid: i32 = text.parse().map_err(io::Error::other)?;
        require(
            pid > 0 && pid.to_string() == text && s.property("ExecMainPID") == Some(text),
            "source authority original main/exec PID differs",
        )?;
        let cgroup = s
            .property("ControlGroup")
            .ok_or_else(|| io::Error::other("source authority cgroup absent"))?;
        require(
            cgroup.starts_with(&format!(
                "/user.slice/user-{}.slice/user@{}.service/",
                self.uid, self.uid
            )) && cgroup.ends_with(&format!("/{}", self.unit))
                && !cgroup.split('/').any(|s| matches!(s, "." | "..")),
            "source authority is not the exact separate user-manager unit",
        )?;
        Ok(Identity {
            pid,
            invocation: invocation.to_owned(),
            cgroup: cgroup.to_owned(),
        })
    }
    fn capture_native(&mut self) -> io::Result<()> {
        let identity = self.identity.as_ref().unwrap();
        require(
            self.pidfd.is_none() && self.directory.is_none(),
            "source authority cannot recapture native objects",
        )?;
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        self.pidfd = Some(unsafe { OwnedFd::from_raw_fd(raw as i32) });
        pidfd_matches(self.pidfd.as_ref().unwrap().as_raw_fd(), identity.pid)?;
        let path =
            CString::new(format!("/sys/fs/cgroup{}", identity.cgroup)).map_err(io::Error::other)?;
        let raw = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        self.directory = Some(unsafe { OwnedFd::from_raw_fd(raw) });
        self.directory_identity = Some(stat(raw)?);
        self.check_native_live()
    }
    fn check_native_live(&self) -> io::Result<()> {
        self.capture_bound()?;
        let launcher = self.launcher.as_ref().unwrap();
        require(
            launcher.reaped.is_none(),
            "source authority original wrapper already reaped",
        )?;
        pidfd_matches(
            launcher
                .pidfd
                .as_ref()
                .ok_or_else(|| io::Error::other("source authority wrapper pidfd absent"))?
                .as_raw_fd(),
            launcher.child.id() as i32,
        )?;
        let identity = self.identity.as_ref().unwrap();
        let pin = self.pidfd.as_ref().unwrap().as_raw_fd();
        pidfd_matches(pin, identity.pid)?;
        let fd = self.directory.as_ref().unwrap().as_raw_fd();
        let held = self.directory_identity.as_ref().unwrap();
        require(
            filesystem(fd)? == 0x6367_7270
                && stat(fd)?.same_object(held)
                && unsafe { libc::fcntl(fd, libc::F_GETFD) } == libc::FD_CLOEXEC
                && unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_ACCMODE == libc::O_RDONLY,
            "source authority original cgroup handle differs",
        )?;
        let path = CString::new(format!("/sys/fs/cgroup{}", identity.cgroup)).unwrap();
        let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::lstat(path.as_ptr(), named.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        require(
            FileIdentity::from(unsafe { named.assume_init() }).same_object(held),
            "source authority cgroup pathname replaced",
        )?;
        require(
            read_file(&format!("/proc/{}/cgroup", identity.pid), 4096)?
                == format!("0::{}\n", identity.cgroup)
                && read_at(fd, "cgroup.procs")?
                    .lines()
                    .any(|p| p == identity.pid.to_string()),
            "source authority actual membership differs",
        )?;
        let ours = read_file("/proc/self/cgroup", 4096)?;
        require(
            ours != format!("0::{}\n", identity.cgroup),
            "source authority shares enclosing CLI cgroup",
        )?;
        for (name, value) in [
            ("memory.max", "268435456\n"),
            ("memory.swap.max", "0\n"),
            ("pids.max", "8\n"),
            ("cpu.max", "100000 100000\n"),
        ] {
            require(
                read_at(fd, name)? == value,
                "source authority actual cgroup resource bound differs",
            )?;
        }
        check_source_authority_policy(identity.pid, self.uid, self.gid)?;
        let expected: Vec<u8> = self
            .image
            .arguments
            .iter()
            .flat_map(|a| a.as_bytes().iter().copied().chain(std::iter::once(0)))
            .collect();
        let mut actual = Vec::new();
        std::fs::File::open(format!("/proc/{}/cmdline", identity.pid))?
            .take(65537)
            .read_to_end(&mut actual)?;
        require(
            actual == expected,
            "source authority actual private argv differs",
        )?;
        let held = stat(self.image.file.as_raw_fd())?;
        let original = self.image.identity.as_ref().unwrap();
        require(
            held.same_owner(original) && held.size == original.size,
            "source authority original held image changed",
        )?;
        pidfd_matches(pin, identity.pid)?;
        self.capture_bound()?;
        Ok(())
    }
    pub fn progress(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.capture_bound()?;
            require(self.started, "source authority launch not started")?;
            self.launcher.as_mut().unwrap().drain()?;
            if self.captured {
                self.check_native_live()?;
                return Ok(true);
            }
            if self.entry.is_some() && self.entry_snapshot.is_none() {
                let deadline = self.capture_bound()?;
                self.entry_snapshot = self.entry.as_mut().unwrap().poll(deadline)?;
                let Some(actual) = &self.entry_snapshot else {
                    return Ok(false);
                };
                require(
                    actual.pid == self.identity.as_ref().unwrap().pid
                        && self.image.digest == Some(actual.digest),
                    "source authority actual executable differs from original held image",
                )?;
                self.check_native_live()?;
                self.start_query(self.stage)?;
            }
            let Some(index) = self.poll_query(self.stage)? else {
                return Ok(false);
            };
            let s = &self.snapshots[index];
            let pending = s.unit() == self.unit
                && s.property("Id") == Some(self.unit.as_str())
                && [
                    ("LoadState", "loaded"),
                    ("ActiveState", "inactive"),
                    ("SubState", "dead"),
                    ("InvocationID", ""),
                    ("MainPID", "0"),
                    ("ExecMainPID", "0"),
                    ("ExecMainCode", "0"),
                    ("ExecMainStatus", "0"),
                    ("Result", "success"),
                    ("ControlGroup", ""),
                    ("TasksCurrent", "[not set]"),
                ]
                .into_iter()
                .all(|(key, value)| s.property(key) == Some(value));
            let starting = s.property("ActiveState") == Some("activating")
                && s.property("SubState") == Some("start");
            if self.identity.is_none() && (pending || starting) {
                if pending {
                    require(
                        self.starting.is_none(),
                        "source authority regressed after original starting identity",
                    )?;
                } else {
                    let value = self.identity(s, true)?;
                    require(
                        s.property("TasksCurrent") == Some("1"),
                        "source authority starting task population differs",
                    )?;
                    if let Some(old) = &self.starting {
                        require(
                            *old == value,
                            "source authority pending invocation/PID replaced",
                        )?;
                    } else {
                        self.starting = Some(value);
                    }
                }
                require(
                    self.queries.len() + self.commands.len() + 6 <= 64,
                    "source authority readiness leaves no original capture/retirement budget",
                )?;
                self.start_query(self.stage)?;
                return Ok(false);
            }
            let identity = self.identity(s, false)?;
            if let Some(old) = &self.starting {
                require(
                    *old == identity,
                    "source authority live identity replaced starting observation",
                )?;
            }
            if let Some(old) = &self.identity {
                require(
                    *old == identity,
                    "source authority changed during native capture",
                )?;
                self.live_snapshots.push(index);
                self.check_native_live()?;
                self.captured = true;
                return Ok(true);
            }
            self.identity = Some(identity);
            self.live_snapshots.push(index);
            self.capture_native()?;
            self.count()?;
            self.entry = Some(EntryQuery::retain(self.identity.as_ref().unwrap().pid));
            self.entry.as_mut().unwrap().start()?;
            Ok(false)
        })();
        self.remember(result)
    }
    pub fn credentials(&self) -> io::Result<wire::Credentials> {
        require(
            self.captured && self.live_snapshots.len() == 2,
            "source authority original double capture incomplete",
        )?;
        self.check_native_live()?;
        Ok(wire::Credentials {
            pid: self.identity.as_ref().unwrap().pid,
            uid: self.uid,
            gid: self.gid,
        })
    }
    pub fn pidfd(&self) -> io::Result<BorrowedFd<'_>> {
        self.credentials()?;
        Ok(self.pidfd.as_ref().unwrap().as_fd())
    }
    fn command(&mut self, verb: &str, cutoff: u64) -> io::Result<()> {
        self.bound(cutoff)?;
        self.count()?;
        require(
            matches!(verb, "stop" | "reset-failed"),
            "source authority unknown retirement command",
        )?;
        self.commands.push(CommandQuery::retain(vec![
            "--user".into(),
            "--no-ask-password".into(),
            verb.into(),
            self.unit.clone(),
        ]));
        let index = self.commands.len() - 1;
        let mut command = self.user_command("/usr/bin/systemctl")?;
        command
            .args(&self.commands[index].arguments)
            .stdin(Stdio::null())
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
        self.commands[index].spawn_retained(&mut command)?;
        self.commands[index].initialize_child()?;
        loop {
            let deadline = self.bound(cutoff)?;
            self.launcher.as_mut().unwrap().drain()?;
            if self.commands[index].poll(deadline)? {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn current(&mut self, cutoff: u64) -> io::Result<usize> {
        self.start_query(cutoff)?;
        loop {
            self.bound(cutoff)?;
            self.launcher.as_mut().unwrap().drain()?;
            if let Some(index) = self.poll_query(cutoff)? {
                return Ok(index);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    /// Wait only for actual native terminality; retain original failure and all
    /// outputs before the exact stop/reset. No successful admission is inferred.
    pub fn retire(&mut self, caller_cutoff: u64) -> io::Result<bool> {
        let result = self.retire_inner(caller_cutoff);
        self.remember(result)
    }
    fn retire_inner(&mut self, caller_cutoff: u64) -> io::Result<bool> {
        let mut cutoff = caller_cutoff.min(self.stage);
        if let Some(origin) = self.failure_origin {
            cutoff = cutoff.min(
                origin
                    .checked_add(1_000_000_000)
                    .ok_or_else(|| io::Error::other("source authority failure bound overflow"))?,
            );
        }
        self.retirement_cutoff = Some(self.retirement_cutoff.map_or(cutoff, |old| old.min(cutoff)));
        let mut cutoff = self.retirement_cutoff.unwrap();
        self.bound(cutoff)?;
        require(
            self.captured,
            "source authority retirement lacks original captured unit",
        )?;
        if self.retired {
            if let Some(error) = &self.refusal {
                return Err(error.error());
            }
            return Ok(true);
        }
        self.launcher.as_mut().unwrap().drain()?;
        if !terminal(self.pidfd.as_ref().unwrap().as_raw_fd())? {
            return Ok(false);
        }
        let native = read_retained_cgroup(
            self.pidfd.as_ref().unwrap().as_fd(),
            self.directory.as_ref().unwrap().as_fd(),
            self.directory_identity.as_ref().unwrap(),
        )?;
        if native.terminal_progress()? == TerminalProgress::Pending {
            return Ok(false);
        }
        if self.terminal_snapshot.is_none() {
            let index = self.current(cutoff)?;
            self.terminal_snapshot = Some(index);
            let snapshot = &self.snapshots[index];
            let identity = self.identity.as_ref().unwrap();
            let pid = identity.pid.to_string();
            require(
                snapshot.property("Id") == Some(self.unit.as_str())
                    && snapshot.property("LoadState") == Some("loaded")
                    && snapshot.property("InvocationID") == Some(identity.invocation.as_str())
                    && snapshot.property("MainPID") == Some("0")
                    && snapshot.property("ExecMainPID") == Some(pid.as_str())
                    && snapshot.property("ExecMainCode") == Some("1")
                    && snapshot
                        .property("ControlGroup")
                        .is_some_and(|g| g.is_empty() || g == identity.cgroup),
                "source authority terminal manager changed original identity/natural wait",
            )?;
            let status = snapshot
                .property("ExecMainStatus")
                .ok_or_else(|| io::Error::other("source authority terminal status absent"))?;
            let code: u32 = status.parse().map_err(io::Error::other)?;
            require(
                code <= 255 && code.to_string() == status,
                "source authority terminal status malformed",
            )?;
            let success = snapshot.property("ActiveState") == Some("active")
                && snapshot.property("SubState") == Some("exited")
                && snapshot.property("Result") == Some("success")
                && code == 0;
            let failed = snapshot.property("ActiveState") == Some("failed")
                && snapshot.property("SubState") == Some("failed")
                && snapshot.property("Result") == Some("exit-code")
                && (1..=255).contains(&code);
            require(
                success || failed,
                "source authority terminal state/result/status combination differs",
            )?;
            if failed {
                let error = io::Error::other(format!(
                    "source authority failed: {} status {code}",
                    snapshot.property("Result").unwrap_or("missing")
                ));
                if self.refusal.is_none() {
                    self.refusal = Some(Failure::capture(&error));
                    self.failure_origin = Some(guardian::monotonic_ns()?);
                }
                cutoff = cutoff.min(
                    self.failure_origin
                        .unwrap()
                        .checked_add(1_000_000_000)
                        .ok_or_else(|| {
                            io::Error::other("source authority failed terminal bound overflow")
                        })?,
                );
                self.retirement_cutoff = Some(cutoff);
                self.bound(cutoff)?;
            }
            let bytes = journal::canonical(
                &json!({"schema":"hermit-source-authority-terminal-before-mutation-v1",
                "unit":self.unit,"snapshot":snapshot.evidence(),"query":self.queries.last().unwrap().evidence(),
                "original_cutoff":cutoff,"first_failure_origin":self.failure_origin}),
            )?;
            require(
                bytes.len() <= 65536,
                "source authority terminal receipt bound",
            )?;
            use std::io::Write;
            let mut stderr = std::io::stderr().lock();
            stderr.write_all(&bytes)?;
            stderr.write_all(b"\n")?;
            stderr.flush()?;
            self.bound(cutoff)?;
            self.terminal_ready = true;
        }
        // Retaining a rejected snapshot is custody, not mutation authority;
        // a repeated retirement call cannot skip its earlier refusal.
        require(
            self.terminal_ready,
            "source authority terminal mutation premise was not completed",
        )?;
        // Once observed, failed result remains sticky even if reset unloads it.
        if !self.stop_attempted && !self.reset_attempted {
            let failed = self.snapshots[self.terminal_snapshot.unwrap()].property("ActiveState")
                == Some("failed");
            if failed {
                self.reset_attempted = true;
                self.command("reset-failed", cutoff)?;
            } else {
                self.stop_attempted = true;
                self.command("stop", cutoff)?;
            }
        }
        let index = self.current(cutoff)?;
        let snapshot = &self.snapshots[index];
        if snapshot.property("LoadState") != Some("not-found") {
            require(
                snapshot.property("InvocationID")
                    == Some(self.identity.as_ref().unwrap().invocation.as_str()),
                "source authority unit replaced during retirement",
            )?;
            return Ok(false);
        }
        require(
            snapshot.property("Id") == Some(self.unit.as_str())
                && snapshot.property("InvocationID") == Some("")
                && snapshot.property("MainPID") == Some("0")
                && snapshot.property("ControlGroup") == Some(""),
            "source authority absent unit fields differ",
        )?;
        let native = read_retained_cgroup(
            self.pidfd.as_ref().unwrap().as_fd(),
            self.directory.as_ref().unwrap().as_fd(),
            self.directory_identity.as_ref().unwrap(),
        )?;
        require(
            matches!(native,CgroupReadbackProgress::Observed(ref state) if state.creator_terminal&&state.unlinked),
            "source authority original cgroup not unlinked",
        )?;
        let path = CString::new(format!(
            "/sys/fs/cgroup{}",
            self.identity.as_ref().unwrap().cgroup
        ))
        .unwrap();
        let mut s = std::mem::MaybeUninit::<libc::stat>::uninit();
        let raw = unsafe { libc::lstat(path.as_ptr(), s.as_mut_ptr()) };
        let errno = (raw == -1).then(io::Error::last_os_error);
        require(
            raw == -1 && errno.as_ref().and_then(io::Error::raw_os_error) == Some(libc::ENOENT),
            "source authority old cgroup pathname remains",
        )?;
        let launcher = self.launcher.as_mut().unwrap();
        launcher.drain()?;
        if launcher.eof != [true, true] || !terminal(launcher.pidfd.as_ref().unwrap().as_raw_fd())?
        {
            return Ok(false);
        }
        if self.wait_observation.is_none() {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let raw = unsafe {
                libc::waitid(
                    libc::P_PID,
                    launcher.child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if raw != 0 {
                return Err(io::Error::last_os_error());
            }
            self.wait_observation = Some(
                json!({"raw":raw,"pid":unsafe{info.si_pid()},"code":info.si_code,"status":unsafe{info.si_status()}}),
            );
            require(
                unsafe { info.si_pid() } == launcher.child.id() as i32
                    && info.si_code == libc::CLD_EXITED,
                "source authority wrapper lacks original natural WNOWAIT",
            )?;
        }
        if self.wait_observation.as_ref().unwrap()["status"] == 0 {
            // The CLI has other real children. Reuse its scoped original-Child
            // join; the isolated source-helper global ECHILD stays elsewhere.
            if !super::runtime_parent::GroupedParentOwner::join_child(
                launcher,
                self.logs.as_raw_fd(),
                cutoff,
            )? {
                return Ok(false);
            }
        } else if launcher.reaped.is_none() {
            launcher.reaped = launcher.child.try_wait()?;
        }
        let wait = launcher
            .reaped
            .ok_or_else(|| io::Error::other("source authority original wrapper wait absent"))?;
        require(
            wait.code().map(i64::from)
                == self.wait_observation.as_ref().unwrap()["status"].as_i64(),
            "source authority consumed wait differs from WNOWAIT",
        )?;
        let raw = unsafe { libc::kill(-(launcher.child.id() as i32), 0) };
        let error = (raw == -1).then(io::Error::last_os_error);
        require(
            raw == -1 && error.as_ref().and_then(io::Error::raw_os_error) == Some(libc::ESRCH),
            "source authority wrapper group remains",
        )?;
        for fd in launcher.log_files.iter().flatten() {
            if unsafe { libc::fsync(fd.as_raw_fd()) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if unsafe { libc::fsync(self.logs.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        launcher.logs_synced = true;
        if !wait.success() && self.refusal.is_none() {
            self.refusal = Some(Failure::capture(&io::Error::other(format!(
                "source authority original wrapper failed {}",
                wait.into_raw()
            ))));
            self.failure_origin = Some(guardian::monotonic_ns()?);
        }
        // Every actual metadata/mutation subprocess remains retained and
        // must have completed its own native wait/EOF/group checks. Unit
        // absence cannot forgive a partially started or failed command owner.
        for query in &self.queries {
            query.completed_custody(self.deadline)?;
        }
        for command in &self.commands {
            command.completed_custody(self.deadline)?;
        }
        self.entry
            .as_ref()
            .ok_or_else(|| io::Error::other("source authority entry query absent"))?
            .completed_custody(self.deadline)?;
        self.bound(cutoff)?;
        self.retired = true;
        if let Some(error) = &self.refusal {
            return Err(error.error());
        }
        Ok(true)
    }
    pub fn drain(&mut self) -> io::Result<()> {
        if let Some(launcher) = &mut self.launcher {
            launcher.drain()?;
        }
        Ok(())
    }
    pub fn completed(&self) -> bool {
        self.retired && self.refusal.is_none()
    }
    /// Actual completed local custody only. A retained failed result stays
    /// failed; this readback cannot satisfy completed() or successful startup.
    pub fn retired_observation(&self) -> Option<Value> {
        self.retired.then(|| self.diagnostics())
    }
    pub fn diagnostics(&self) -> Value {
        json!({"unit":self.unit,"nonce":self.nonce,"manager":"user","captured":self.captured,
        "pid":self.identity.as_ref().map(|v|v.pid),"invocation":self.identity.as_ref().map(|v|&v.invocation),
        "cgroup":self.identity.as_ref().map(|v|&v.cgroup),"stage_deadline":self.stage,"retirement_cutoff":self.retirement_cutoff,
        "queries":self.queries.iter().map(ManagerQuery::evidence).collect::<Vec<_>>(),
        "snapshots":self.snapshots.iter().map(ManagerSnapshot::evidence).collect::<Vec<_>>(),
        "commands":self.commands.iter().map(CommandQuery::evidence).collect::<Vec<_>>(),
        "entry_query":self.entry.as_ref().map(EntryQuery::evidence),"waitid":self.wait_observation,
        "original_wrapper":self.launcher.as_ref().map(|launcher|json!({"pid":launcher.child.id(),
            "raw_wait_status":launcher.reaped.map(ExitStatus::into_raw),"eof":launcher.eof,"logs_synced":launcher.logs_synced,
            "stdout_bytes":launcher.stdout.len(),"stderr_bytes":launcher.stderr.len()})),
        "retired":self.retired,"first_failure":self.refusal.as_ref().map(|f|&f.message),"failure_origin":self.failure_origin,
        "global_ECHILD_claimed":false})
    }
}
