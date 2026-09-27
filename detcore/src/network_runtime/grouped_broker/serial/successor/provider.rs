//! Original CLI launch capture, transported over its actual private endpoint.
//! This is a retained comparison obligation, not a deserialized Creator issuer.
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;

use serde::Deserialize;
use serde::Serialize;

use super::super::super::journal;
use super::super::super::owner;
use super::super::super::require;
use super::super::super::wire;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Capture {
    schema: String,
    run: String,
    stage_deadline: u64,
    unit: String,
    invocation: String,
    pid: i32,
    cgroup: String,
    device: u64,
    inode: u64,
    argv: Vec<String>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Image {
    schema: String,
    run: String,
    stage_deadline: u64,
}

#[derive(Debug)]
pub(in super::super::super) struct ProviderIdentity {
    capture: Capture,
    pidfd: OwnedFd,
    directory: OwnedFd,
    image: owner::EntryImage,
    initialized: bool,
}
impl ProviderIdentity {
    pub fn unit(&self) -> &str {
        &self.capture.unit
    }
    pub fn arguments(&self) -> Vec<std::ffi::OsString> {
        self.capture
            .argv
            .iter()
            .map(std::ffi::OsString::from)
            .collect()
    }
    pub fn peer(&self) -> wire::Credentials {
        wire::Credentials {
            pid: self.capture.pid,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }
    /// Call only with the two packets on the original CLI-owned channel, whose
    /// peer credentials were retained before any transfer. Grammar errors keep
    /// rights in those packets. After taking rights there is no fallible work
    /// before returning the object for immediate installation in its owner.
    pub fn retain(
        capture: &mut wire::Packet,
        image: &mut wire::Packet,
        original_cli: wire::Credentials,
        run: &str,
        deadline: u64,
    ) -> io::Result<Self> {
        capture.exact(2, original_cli)?;
        image.exact(1, original_cli)?;
        let value: Capture = serde_json::from_slice(&capture.bytes)?;
        let expected_image: Image = serde_json::from_slice(&image.bytes)?;
        require(
            journal::canonical(&serde_json::to_value(&value)?)? == capture.bytes
                && journal::canonical(&serde_json::to_value(&expected_image)?)? == image.bytes,
            "original provider capture framing differs",
        )?;
        require(
            value.schema == "hermit-grouped-provider-capture-v1"
                && value.run == run
                && value.stage_deadline == deadline
                && value.pid > 0
                && super::super::super::valid_nonce(&value.invocation)
                && value
                    .unit
                    .strip_prefix("hermit-accepted-")
                    .and_then(|s| s.strip_suffix(".service"))
                    .is_some_and(super::super::super::valid_nonce)
                && value.cgroup.starts_with('/')
                && value.cgroup != "/"
                && !value.cgroup.split('/').any(|s| matches!(s, "." | ".."))
                && !value.argv.is_empty()
                && value.argv.len() <= 128
                && value.argv.iter().all(|s| !s.as_bytes().contains(&0)),
            "original provider capture replaced the launch identity",
        )?;
        require(
            expected_image.schema == "hermit-grouped-provider-image-v1"
                && expected_image.run == run
                && expected_image.stage_deadline == deadline,
            "original provider image transfer replaced the launch",
        )?;
        let arguments = value.argv.iter().map(std::ffi::OsString::from).collect();
        Ok(Self {
            capture: value,
            pidfd: capture.rights.remove(0),
            directory: capture.rights.remove(0),
            image: owner::EntryImage::retain(image.rights.remove(0), arguments),
            initialized: false,
        })
    }
    pub fn initialize(
        &mut self,
        unit: &str,
        arguments: &[std::ffi::OsString],
        run: &str,
        deadline: u64,
    ) -> io::Result<()> {
        require(
            !self.initialized
                && self.capture.unit == unit
                && self.capture.run == run
                && self.capture.stage_deadline == deadline
                && self
                    .capture
                    .argv
                    .iter()
                    .map(std::ffi::OsString::from)
                    .collect::<Vec<_>>()
                    == arguments,
            "S2 original capture differs from the retained leaf request",
        )?;
        self.check_live()?;
        self.image.initialize()?;
        self.initialized = true;
        Ok(())
    }
    pub fn check_bound(
        &self,
        unit: &str,
        arguments: &[std::ffi::OsString],
        run: &str,
        deadline: u64,
    ) -> io::Result<()> {
        require(
            self.initialized
                && self.capture.unit == unit
                && self.capture.run == run
                && self.capture.stage_deadline == deadline
                && self.arguments() == arguments,
            "S2 original provider preparation changed",
        )?;
        self.check_live()
    }
    fn check_live(&self) -> io::Result<()> {
        require(
            super::super::super::guardian::monotonic_ns()? < self.capture.stage_deadline,
            "S2 provider original capture deadline expired",
        )?;
        owner::pidfd_matches(self.pidfd.as_raw_fd(), self.capture.pid)?;
        require(
            !owner::terminal(self.pidfd.as_raw_fd())?
                && owner::filesystem(self.pidfd.as_raw_fd())? == 0x5049_4446
                && owner::filesystem(self.directory.as_raw_fd())? == 0x6367_7270,
            "S2 original provider native handles are not live",
        )?;
        for fd in [self.pidfd.as_raw_fd(), self.directory.as_raw_fd()] {
            require(
                unsafe { libc::fcntl(fd, libc::F_GETFD) } == libc::FD_CLOEXEC,
                "S2 provider native capture lacks CLOEXEC",
            )?;
        }
        let flags = unsafe { libc::fcntl(self.directory.as_raw_fd(), libc::F_GETFL) };
        let actual = owner::stat(self.directory.as_raw_fd())?;
        require(
            flags >= 0
                && flags & libc::O_ACCMODE == libc::O_RDONLY
                && actual.mode & libc::S_IFMT == libc::S_IFDIR
                && actual.device == self.capture.device
                && actual.inode == self.capture.inode,
            "S2 original provider cgroup description changed",
        )?;
        require(
            owner::read_file(&format!("/proc/{}/cgroup", self.capture.pid), 4096)?
                == format!("0::{}\n", self.capture.cgroup),
            "S2 original provider cgroup membership changed",
        )?;
        Ok(())
    }
    pub fn authenticate(
        &self,
        creator: &mut owner::Creator,
        manager: &owner::ManagerSnapshot,
        image: &owner::EntrySnapshot,
    ) -> io::Result<()> {
        require(
            self.initialized,
            "original provider image was not initialized",
        )?;
        self.check_live()?;
        creator.authenticate(manager, &self.image, image)?;
        self.matches_creator(creator)
    }
    pub fn matches_creator(&self, creator: &owner::Creator) -> io::Result<()> {
        self.check_live()?;
        require(
            self.initialized && creator.admitted && creator.peer.pid == self.capture.pid,
            "actual S2 Creator differs from the original trusted launch",
        )?;
        owner::pidfd_matches(creator.pidfd.as_raw_fd(), self.capture.pid)?;
        require(
            owner::stat(creator.directory.as_raw_fd())?
                .same_object(&owner::stat(self.directory.as_raw_fd())?),
            "S2 Creator did not retain the original captured cgroup",
        )?;
        let observed = creator.evidence()?;
        for (name, expected) in [
            ("unit", serde_json::json!(self.capture.unit)),
            ("invocation", serde_json::json!(self.capture.invocation)),
            ("pid", serde_json::json!(self.capture.pid)),
            ("cgroup", serde_json::json!(self.capture.cgroup)),
            ("device", serde_json::json!(self.capture.device)),
            ("inode", serde_json::json!(self.capture.inode)),
        ] {
            require(
                observed[name] == expected,
                "S2 independent Creator capture differs from original launch",
            )?;
        }
        Ok(())
    }
    pub fn descriptors(&self) -> [i32; 2] {
        [self.pidfd.as_raw_fd(), self.directory.as_raw_fd()]
    }
}
