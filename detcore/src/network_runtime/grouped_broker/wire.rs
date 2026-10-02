//! Private kernel-authenticated packets. Received rights enter retained custody
//! before packet validation; protocol failure never discards those owners.
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;

use super::Failure;
use super::require;

pub(super) const MAX_PACKET: usize = 65_536;
const MAX_RECEIPTS: usize = 128;
const MAX_BYTES: usize = 1_048_576;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Credentials {
    pub pid: libc::pid_t,
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RawCall {
    pub returned: isize,
    pub errno: Option<i32>,
}

#[derive(Debug)]
pub(super) struct Packet {
    pub bytes: Vec<u8>,
    pub rights: Vec<OwnedFd>,
    pub credentials: Vec<Credentials>,
    pub rights_messages: usize,
    pub flags: i32,
    pub raw: RawCall,
    malformed: bool,
}
impl Packet {
    pub fn exact(&self, rights: usize, peer: Credentials) -> io::Result<()> {
        require(
            !self.malformed
                && self.raw.returned > 0
                && self.raw.errno.is_none()
                && self.flags == libc::MSG_CMSG_CLOEXEC
                && self.rights.len() == rights
                && self.rights_messages == usize::from(rights != 0)
                && self.credentials == [peer],
            "grouped packet credentials, flags or rights differ",
        )
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "{error}; malformed={}; returned={}; errno={:?}; flags={:#x}; expected_flags={:#x}; rights={}; expected_rights={}; rights_messages={}; expected_rights_messages={}; credentials={:?}; expected_peer={:?}; bytes={}",
                    self.malformed,
                    self.raw.returned,
                    self.raw.errno,
                    self.flags,
                    libc::MSG_CMSG_CLOEXEC,
                    self.rights.len(),
                    rights,
                    self.rights_messages,
                    usize::from(rights != 0),
                    self.credentials,
                    peer,
                    self.bytes.len(),
                ),
            )
        })?;
        for fd in &self.rights {
            require(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } == libc::FD_CLOEXEC,
                "grouped received alias lacks CLOEXEC",
            )?;
        }
        Ok(())
    }
}

/// One atomic creation-readiness transfer: three original control descriptions
/// followed by the original source ledger directory and file. These borrows
/// express wire roles only; the caller still proves native custody/readiness.
/// This distinct type does not widen the legacy generic three-right send API.
pub(super) struct CreationLedgerBundle<'a> {
    controls: [BorrowedFd<'a>; 3],
    source_store: [BorrowedFd<'a>; 2],
}
impl<'a> CreationLedgerBundle<'a> {
    pub(super) fn new(controls: [BorrowedFd<'a>; 3], source_store: [BorrowedFd<'a>; 2]) -> Self {
        Self {
            controls,
            source_store,
        }
    }
}

#[derive(Debug)]
pub(super) struct SendAttempt {
    pub bytes: Vec<u8>,
    pub rights: Vec<i32>,
    pub raw: Option<RawCall>,
}

#[derive(Debug)]
pub(super) struct Channel {
    pub fd: OwnedFd,
    pub packets: Vec<Packet>,
    pub sends: Vec<SendAttempt>,
    pub last_receive: Option<RawCall>,
    pub refused: Option<Failure>,
    received_bytes: usize,
    #[expect(dead_code, reason = "Original preconfiguration EOF refusal protocol is retained but not yet integrated")]
    pub uninitialized_eof_options: Option<[i32; 3]>,
}
impl Channel {
    /// Infallible: move actual ownership into the recovery state before I/O.
    pub fn retain(fd: OwnedFd) -> Self {
        Self {
            fd,
            packets: Vec::new(),
            sends: Vec::new(),
            last_receive: None,
            refused: None,
            received_bytes: 0,
            uninitialized_eof_options: None,
        }
    }
    pub fn validate(&self) -> io::Result<()> {
        require(self.refused.is_none(), "grouped channel already refused")?;
        for (option, expected) in [
            (libc::SO_TYPE, libc::SOCK_SEQPACKET),
            (libc::SO_PASSCRED, 1),
        ] {
            let mut actual = 0i32;
            let mut size = std::mem::size_of_val(&actual) as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    self.fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    (&mut actual as *mut i32).cast(),
                    &mut size,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            require(
                size as usize == std::mem::size_of_val(&actual) && actual == expected,
                "grouped channel type/credential option changed",
            )?;
        }
        let flags = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_GETFL) };
        require(
            flags >= 0 && flags & libc::O_NONBLOCK != 0,
            "grouped channel must be nonblocking",
        )
    }
    pub fn receive(&mut self, cap: usize) -> io::Result<Option<usize>> {
        let result = self.receive_inner(cap);
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn receive_inner(&mut self, cap: usize) -> io::Result<Option<usize>> {
        self.validate()?;
        self.receive_packet(cap)
    }
    /// Custody only for the Keeper's never-forwarded local pair after its
    /// actual first PASSCRED setup call failed. The caller has closed the only
    /// original peer and proved it never sent either endpoint. This method
    /// permanently refuses normal protocol use; it returns only exact EOF,
    /// never an accepted packet or source-admission capability.
    #[expect(dead_code, reason = "Original preconfiguration EOF refusal protocol is retained but not yet integrated")]
    pub fn receive_uninitialized_eof(&mut self, setup: RawCall) -> io::Result<bool> {
        require(
            setup.returned == -1 && setup.errno.is_some(),
            "uninitialized EOF lacks actual setup failure",
        )?;
        self.refused.get_or_insert_with(|| {
            Failure::capture(&io::Error::from_raw_os_error(setup.errno.unwrap()))
        });
        require(
            self.refused.as_ref().unwrap().errno == setup.errno
                && self.packets.is_empty()
                && self.sends.is_empty()
                && self.received_bytes == 0,
            "uninitialized EOF already has protocol traffic or a different refusal",
        )?;
        let mut options = [0i32; 3];
        for (index, option, expected) in [
            (0, libc::SO_TYPE, libc::SOCK_SEQPACKET),
            (1, libc::SO_PASSCRED, 0),
        ] {
            let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    self.fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    (&mut options[index] as *mut i32).cast(),
                    &mut size,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            require(
                size as usize == std::mem::size_of::<i32>() && options[index] == expected,
                "uninitialized EOF socket option differs from failed local setup",
            )?;
        }
        options[2] = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_GETFL) };
        require(
            options[2] >= 0 && options[2] & libc::O_NONBLOCK != 0,
            "uninitialized EOF channel is blocking",
        )?;
        self.uninitialized_eof_options = Some(options);
        let Some(index) = self.receive_packet(1536)? else {
            return Ok(false);
        };
        let packet = &self.packets[index];
        require(
            !packet.malformed
                && packet.raw.returned == 0
                && packet.raw.errno.is_none()
                && packet.bytes.is_empty()
                && packet.credentials.is_empty()
                && packet.rights.is_empty()
                && packet.rights_messages == 0
                && packet.flags == libc::MSG_CMSG_CLOEXEC,
            "uninitialized EOF retained unexpected packet or rights",
        )?;
        Ok(true)
    }
    fn receive_packet(&mut self, cap: usize) -> io::Result<Option<usize>> {
        require(
            cap > 0 && cap <= MAX_PACKET && self.packets.len() < MAX_RECEIPTS,
            "grouped packet bound exhausted",
        )?;
        let mut packet = Packet {
            bytes: vec![0; cap],
            rights: Vec::new(),
            credentials: Vec::new(),
            rights_messages: 0,
            flags: 0,
            raw: RawCall {
                returned: -1,
                errno: None,
            },
            malformed: false,
        };
        // Word alignment for cmsghdr, enough for every required protocol role.
        // Kernel MSG_CTRUNC closes undisclosed extra rights; disclosed aliases
        // are all retained below before the truncation is rejected.
        let mut control = [0usize; 32];
        let mut iov = libc::iovec {
            iov_base: packet.bytes.as_mut_ptr().cast(),
            iov_len: cap,
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&control);
        let raw = unsafe {
            libc::recvmsg(
                self.fd.as_raw_fd(),
                &mut message,
                libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
            )
        };
        let error = (raw < 0).then(io::Error::last_os_error);
        packet.raw = RawCall {
            returned: raw,
            errno: error.as_ref().and_then(io::Error::raw_os_error),
        };
        self.last_receive = Some(packet.raw);
        if let Some(error) = error {
            return if error.kind() == io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(error)
            };
        }
        packet.flags = message.msg_flags;
        packet.bytes.truncate(raw as usize);
        let mut header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        while !header.is_null() {
            let current = unsafe { &*header };
            let base = unsafe { libc::CMSG_LEN(0) } as usize;
            if current.cmsg_len < base {
                packet.malformed = true;
                break;
            }
            let bytes = current.cmsg_len - base;
            let data = unsafe { libc::CMSG_DATA(header) };
            if current.cmsg_level == libc::SOL_SOCKET && current.cmsg_type == libc::SCM_RIGHTS {
                packet.rights_messages += 1;
                packet.malformed |= !bytes.is_multiple_of(std::mem::size_of::<i32>());
                for index in 0..bytes / std::mem::size_of::<i32>() {
                    let fd = unsafe {
                        data.add(index * std::mem::size_of::<i32>())
                            .cast::<i32>()
                            .read_unaligned()
                    };
                    packet.rights.push(unsafe { OwnedFd::from_raw_fd(fd) });
                }
            } else if current.cmsg_level == libc::SOL_SOCKET
                && current.cmsg_type == libc::SCM_CREDENTIALS
                && bytes == std::mem::size_of::<libc::ucred>()
            {
                let c = unsafe { data.cast::<libc::ucred>().read_unaligned() };
                packet.credentials.push(Credentials {
                    pid: c.pid,
                    uid: c.uid,
                    gid: c.gid,
                });
            } else {
                packet.malformed = true;
            }
            header = unsafe { libc::CMSG_NXTHDR(&message, header) };
        }
        self.received_bytes += packet.bytes.len();
        self.packets.push(packet); // custody precedes all grammar validation
        require(
            self.received_bytes <= MAX_BYTES,
            "grouped aggregate packet bound exceeded",
        )?;
        Ok(Some(self.packets.len() - 1))
    }
    pub fn send_once(&mut self, bytes: &[u8], rights: &[BorrowedFd<'_>]) -> io::Result<()> {
        let result = self.send_inner(bytes, rights);
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    /// The creation controller/peer occupy their one-use phase before this
    /// call. Retain one attempted atomic send and its actual native result;
    /// any error latches the same Channel refusal and never retries the send.
    pub(super) fn send_creation_bundle(
        &mut self,
        bytes: &[u8],
        bundle: CreationLedgerBundle<'_>,
    ) -> io::Result<()> {
        let result = (|| {
            self.validate()?;
            require(
                !bytes.is_empty() && bytes.len() <= MAX_PACKET && self.sends.len() < MAX_RECEIPTS,
                "grouped creation bundle send bound",
            )?;
            require(
                (unsafe { libc::CMSG_SPACE(std::mem::size_of::<[i32; 5]>() as u32) }) as usize
                    <= std::mem::size_of::<[usize; 8]>(),
                "grouped creation bundle ancillary bound",
            )?;
            let rights = [
                bundle.controls[0],
                bundle.controls[1],
                bundle.controls[2],
                bundle.source_store[0],
                bundle.source_store[1],
            ];
            self.send_packet(bytes, &rights)
        })();
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn send_inner(&mut self, bytes: &[u8], rights: &[BorrowedFd<'_>]) -> io::Result<()> {
        self.validate()?;
        require(
            !bytes.is_empty()
                && bytes.len() <= MAX_PACKET
                && rights.len() <= 3
                && self.sends.len() < MAX_RECEIPTS,
            "grouped send bound",
        )?;
        self.send_packet(bytes, rights)
    }
    // Both callers validate their distinct fixed arity before entering this
    // unchanged native encoder. No maximum argument or generic exemption.
    fn send_packet(&mut self, bytes: &[u8], rights: &[BorrowedFd<'_>]) -> io::Result<()> {
        self.sends.push(SendAttempt {
            bytes: bytes.to_vec(),
            rights: rights.iter().map(AsRawFd::as_raw_fd).collect(),
            raw: None,
        });
        let attempt = self.sends.last_mut().unwrap();
        let mut control = [0usize; 8];
        let mut iov = libc::iovec {
            iov_base: attempt.bytes.as_mut_ptr().cast(),
            iov_len: attempt.bytes.len(),
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        if !rights.is_empty() {
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = unsafe {
                libc::CMSG_SPACE(std::mem::size_of_val(attempt.rights.as_slice()) as u32)
            } as usize;
            let c = unsafe { libc::CMSG_FIRSTHDR(&message) };
            unsafe {
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len =
                    libc::CMSG_LEN(std::mem::size_of_val(attempt.rights.as_slice()) as u32)
                        as usize;
                std::ptr::copy_nonoverlapping(
                    attempt.rights.as_ptr().cast::<u8>(),
                    libc::CMSG_DATA(c),
                    std::mem::size_of_val(attempt.rights.as_slice()),
                );
            }
        }
        let raw = unsafe {
            libc::sendmsg(
                self.fd.as_raw_fd(),
                &message,
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        let error = (raw < 0).then(io::Error::last_os_error);
        attempt.raw = Some(RawCall {
            returned: raw,
            errno: error.as_ref().and_then(io::Error::raw_os_error),
        });
        if let Some(error) = error {
            return Err(error);
        }
        require(
            raw as usize == bytes.len(),
            "grouped short send remains unknown",
        )
    }
}
