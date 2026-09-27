//! Closed early process entries. Configuration is received through the actual
//! private endpoint; manager inherited roles are moved in by the CLI parser.
//! Neither argv nor these transport records construct source/adoption authority.
use std::ffi::CString;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use super::Intent;
use super::guardian;
use super::journal;
use super::native;
use super::owner;
use super::require;
use super::wire;

mod controller;
pub(super) mod runtime_creation;
mod source;
pub use controller::run_grouped_startup_controller_process;
pub use source::run_grouped_leaf_delegate_process;
pub use source::run_grouped_source_process;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BridgeConfiguration {
    pub schema: String,
    pub nonce: String,
    pub incarnation: u64,
    pub unit: String,
    pub stage_deadline: u64,
    pub bridge_sha256: String,
}

impl BridgeConfiguration {
    pub(super) fn check(&self, intent: &Intent, unit: &str, deadline: u64) -> io::Result<[u8; 32]> {
        require(
            self.schema == "hermit-grouped-source-bridge-v1"
                && self.nonce == intent.nonce
                && self.incarnation == intent.incarnation
                && self.unit == unit
                && self.stage_deadline == deadline,
            "source bridge configuration replaced the original entry",
        )?;
        decode_digest(&self.bridge_sha256)
    }
}

pub(super) fn decode_digest(text: &str) -> io::Result<[u8; 32]> {
    require(
        text.len() == 64
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "bridge digest grammar differs",
    )?;
    let mut bytes = [0; 32];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] =
            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).map_err(io::Error::other)?;
    }
    require(bytes != [0; 32], "bridge digest is absent")?;
    Ok(bytes)
}

/// Each early entry owns this before the first receive. The cutoff is sampled
/// exactly once, before configuration, and is never replaced after a refusal.
struct EntryCustody {
    channel: wire::Channel,
    intent: Intent,
    unit: String,
    deadline: Instant,
    native_deadline: u64,
    creator_cutoff: u64,
    peer: Option<wire::Credentials>,
    peer_pidfd: Option<OwnedFd>,
    creator_pidfd: Option<OwnedFd>,
    creator_directory: Option<OwnedFd>,
    creator_packet: Option<Vec<u8>>,
    creator_sent: bool,
    creator_accepted: bool,
}
impl EntryCustody {
    fn retain(
        channel: OwnedFd,
        unit: String,
        run: [u8; 16],
        incarnation: u64,
        native_deadline: u64,
    ) -> io::Result<Self> {
        let sampled = Instant::now();
        let now = guardian::monotonic_ns()?;
        require(
            native_deadline > now && native_deadline - now <= 20_000_000_000,
            "early grouped entry exceeds original20s startup bound",
        )?;
        require(
            incarnation == u64::from_le_bytes(run[..8].try_into().unwrap()),
            "early grouped incarnation differs from run",
        )?;
        let intent = Intent::new(super::hex(&run), incarnation)?;
        require(
            unit.strip_prefix("hermit-accepted-")
                .and_then(|s| s.strip_suffix(".service"))
                .is_some_and(super::valid_nonce),
            "early grouped unit is not an exact accepted unit",
        )?;
        Ok(Self {
            channel: wire::Channel::retain(channel),
            intent,
            unit,
            deadline: sampled + Duration::from_nanos(native_deadline - now),
            native_deadline,
            creator_cutoff: now
                .checked_add(1_000_000_000)
                .ok_or_else(|| io::Error::other("creator cutoff overflow"))?
                .min(native_deadline),
            peer: None,
            peer_pidfd: None,
            creator_pidfd: None,
            creator_directory: None,
            creator_packet: None,
            creator_sent: false,
            creator_accepted: false,
        })
    }
    fn check(&self, creator_window: bool) -> io::Result<()> {
        let now = guardian::monotonic_ns()?;
        require(
            Instant::now() < self.deadline
                && now < self.native_deadline
                && (!creator_window || now < self.creator_cutoff),
            "early grouped original deadline expired",
        )?;
        if let (Some(peer), Some(pidfd)) = (self.peer, &self.peer_pidfd) {
            owner::pidfd_matches(pidfd.as_raw_fd(), peer.pid)?;
            require(
                !owner::terminal(pidfd.as_raw_fd())?,
                "early grouped endpoint owner is terminal",
            )?;
        }
        Ok(())
    }
    fn initialize(&mut self) -> io::Result<()> {
        self.check(true)?;
        require(
            self.peer.is_none() && self.peer_pidfd.is_none(),
            "early entry initialization repeated",
        )?;
        protect()?;
        exact_service_policy()?;
        self.channel.validate()?;
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                self.channel.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut peer as *mut libc::ucred).cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        require(
            size as usize == std::mem::size_of_val(&peer)
                && peer.pid > 0
                && peer.pid != unsafe { libc::getpid() }
                && peer.uid == unsafe { libc::getuid() }
                && peer.gid == unsafe { libc::getgid() },
            "early entry endpoint credentials differ",
        )?;
        self.peer = Some(wire::Credentials {
            pid: peer.pid,
            uid: peer.uid,
            gid: peer.gid,
        });
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, peer.pid, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        self.peer_pidfd = Some(unsafe { OwnedFd::from_raw_fd(raw as i32) });
        self.check(true)
    }
    fn receive(&mut self, cap: usize) -> io::Result<usize> {
        loop {
            self.check(true)?;
            if let Some(index) = self.channel.receive(cap)? {
                return Ok(index);
            }
            let now = guardian::monotonic_ns()?;
            require(
                now < self.creator_cutoff,
                "early entry original creator cutoff expired",
            )?;
            let mut fd = libc::pollfd {
                fd: self.channel.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let milliseconds = ((self.creator_cutoff - now + 999_999) / 1_000_000) as i32;
            let raw = unsafe { libc::poll(&mut fd, 1, milliseconds) };
            if raw < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            require(
                raw == 1 && fd.revents & libc::POLLIN != 0,
                "early entry received no bounded protocol response",
            )?;
        }
    }
    fn announce_creator(&mut self) -> io::Result<()> {
        self.announce_creator_with_namespace(None)
    }
    fn announce_creator_with_namespace(
        &mut self,
        namespace: Option<BorrowedFd<'_>>,
    ) -> io::Result<()> {
        self.check(true)?;
        require(
            self.creator_pidfd.is_none() && !self.creator_sent,
            "early Creator announcement cannot repeat",
        )?;
        let invocation = std::env::var("INVOCATION_ID").map_err(io::Error::other)?;
        require(
            super::valid_nonce(&invocation),
            "manager invocation is absent or malformed",
        )?;
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        self.creator_pidfd = Some(unsafe { OwnedFd::from_raw_fd(raw as i32) });
        let membership = owner::read_file("/proc/self/cgroup", 4096)?;
        let group = membership
            .strip_prefix("0::")
            .and_then(|s| s.strip_suffix('\n'))
            .ok_or_else(|| io::Error::other("early Creator cgroup framing differs"))?;
        require(
            group.starts_with('/')
                && group != "/"
                && !group.contains('\n')
                && !group.split('/').any(|s| matches!(s, "." | "..")),
            "early Creator cgroup is not an exact nonroot path",
        )?;
        let path = CString::new(format!("/sys/fs/cgroup{group}")).map_err(io::Error::other)?;
        let raw = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        self.creator_directory = Some(unsafe { OwnedFd::from_raw_fd(raw) });
        require(
            owner::filesystem(raw)? == 0x6367_7270,
            "early Creator directory is not actual cgroup2",
        )?;
        let pid = unsafe { libc::getpid() };
        owner::pidfd_matches(self.creator_pidfd.as_ref().unwrap().as_raw_fd(), pid)?;
        self.creator_packet = Some(
            format!(
                "UNIT_CREATED unit={} invocation={invocation} pid={pid} nonce={}\n",
                self.unit, self.intent.nonce
            )
            .into_bytes(),
        );
        self.creator_sent = true;
        self.channel.send_once(
            self.creator_packet.as_ref().unwrap(),
            &[
                self.creator_pidfd.as_ref().unwrap().as_fd(),
                self.creator_directory.as_ref().unwrap().as_fd(),
            ],
        )?;
        if let Some(namespace) = namespace {
            self.check(true)?;
            let value = json!({"schema":"hermit-grouped-source-namespace-v1",
                "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,
                "stage_deadline":self.native_deadline,"unit":self.unit,"pid":pid,
                "creator_cutoff":self.creator_cutoff});
            self.channel
                .send_once(&journal::canonical(&value)?, &[namespace])?;
        }
        let index = self.receive(2048)?;
        let packet = &self.channel.packets[index];
        packet.exact(0, self.peer.unwrap())?;
        require(
            packet.bytes == format!("EXEC {}\n", self.intent.nonce).as_bytes(),
            "early Creator actual owner did not authorize exact EXEC",
        )?;
        self.creator_accepted = true;
        self.check(true)
    }
}

fn protect() -> io::Result<()> {
    require(
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } == 0
            && unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0,
        "early grouped protection failed",
    )?;
    owner::protected_holder()
}
fn exact_service_policy() -> io::Result<()> {
    let peer = wire::Credentials {
        pid: unsafe { libc::getpid() },
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
    };
    require(
        peer.uid != 0,
        "grouped service requires actual unprivileged identity",
    )?;
    owner::validate_process_status(&owner::read_file("/proc/self/status", 16_384)?, peer)?;
    let tasks = std::fs::read_dir("/proc/self/task")?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    require(
        tasks.len() == 1 && tasks[0] == peer.pid.to_string().as_str(),
        "grouped early entry is not the original single thread",
    )?;
    for (resource, expected) in [
        (
            libc::RLIMIT_NOFILE,
            crate::network_runtime::capability_unit::CAPABILITY_UNIT_NOFILE,
        ),
        (libc::RLIMIT_FSIZE, 1_048_576),
        (libc::RLIMIT_CORE, 0),
    ] {
        let mut value: libc::rlimit = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrlimit(resource, &mut value) } != 0 {
            return Err(io::Error::last_os_error());
        }
        require(
            value.rlim_cur == expected && value.rlim_max == expected,
            "grouped early entry original resource limit differs",
        )?;
    }
    Ok(())
}

fn finish_entry(result: io::Result<()>) -> ! {
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("grouped early entry refused: {error}");
            // The externally held native Creator and complete owner journals
            // decide cleanup. Process exit is never reported as global deletion.
            std::process::exit(125)
        }
    }
}
