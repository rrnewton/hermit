/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic ports for implicit binds.
//!
//! Linux binds a TCP or UDP socket that has no local port to one it picks
//! itself, the first time the socket needs one: TCP at `connect(2)` and
//! `listen(2)`, UDP at `connect(2)` and at a send. The pick is random. A TCP
//! `connect` starts from a hash of the addresses and of the host's clock in
//! ten-second steps, keyed by a secret drawn at boot, plus the entry of a
//! boot-random perturbation table that hash selects, which every connect
//! through that entry, in any network namespace, advances
//! (`__inet_hash_connect`); the other picks start at a random offset
//! (`inet_csk_find_open_port`, `udp_lib_get_port`). The guest sees the port
//! in `getsockname(2)`, and its peer in `accept(2)`, `recvfrom(2)` and
//! `getpeername(2)`.
//!
//! In a run that records nothing, Detcore binds such a socket first, to a
//! port from its own allocator (`crate::port_allocator`), the one a bind to
//! port 0 already draws from, and then forwards the guest's call unchanged:
//! the kernel finds the socket bound and picks nothing. The bind keeps the
//! socket's own address, so routing and the source address stay Linux's.
//! A port the kernel refuses, because a socket the allocator does not count
//! holds it, is swapped for the next one, as Linux skips a port in use; when
//! no port is left the call fails with Linux's errno for that call. If
//! Detcore cannot stage the bind, the call goes ahead and Linux picks.
//! Record and replay are unchanged: the recording carries the kernel's port.
//!
//! Where the explicit bind differs from Linux's own:
//! - The kernel keeps a port it was told (`SOCK_BINDPORT_LOCK`), and frees
//!   one it picked when a TCP socket closes or a UDP socket disconnects. So
//!   after a failed TCP connect or a UDP `connect(AF_UNSPEC)` the socket
//!   keeps its port here: `getsockname` still shows it, a later `bind`
//!   fails with `EINVAL`, and a retried connect reuses it.
//! - A TCP connect is bound before Linux would bind it, so a connect that
//!   then fails without a port (no route, a broadcast address, a v4-mapped
//!   address on an IPv6-only socket) leaves the socket bound. A send whose
//!   own arguments fail before Linux binds (a bad address or `msghdr`) does
//!   too.
//! - Linux lets connections to different peers share a local port, and may
//!   reuse a port in `TIME_WAIT` for the same peer after
//!   `tcp_tw_reuse_delay`; an explicit bind conflicts with both, so a range
//!   runs out at its size.
//!
//! Not covered, so Linux still picks: a TCP Fast Open send
//! (`MSG_FASTOPEN`), a UDP `write`/`writev`/`splice` on an unbound socket
//! (which fails with `EDESTADDRREQ` after binding), a socket that arrived
//! over `SCM_RIGHTS` without Detcore having seen its creation, and
//! protocols other than TCP and UDP (UDP-Lite, MPTCP, SCTP).

use std::mem::size_of;

use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use tracing::trace;
use tracing::warn;

use super::files::procfs_levels::map_transient;
use super::files::procfs_levels::unmap_transient;
use crate::fd::PortTransport;
use crate::fd::UnboundSocket;
use crate::record_or_replay::RecordOrReplay;
use crate::syscalls::helpers::SOCK_TYPE_MASK;
use crate::syscalls::network_trace::SOCKADDR_IN6_RFC2133_LEN;
use crate::syscalls::network_trace::read_sockaddr;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::tool_global::send_and_update_time;
use crate::tool_local::Detcore;
use crate::types::OpenFileId;
use crate::types::RawFd;

/// The socket option narrowing the range a socket's port is picked from
/// (`include/uapi/linux/in.h`, Linux 6.3 and later).
const IP_LOCAL_PORT_RANGE: i32 = 51;

/// The socket `socket(domain, type, protocol)` creates, if Linux picks its
/// port when it has none: TCP or UDP over IPv4 or IPv6. Protocol 0 is the
/// first protocol of the type, TCP for a stream and UDP for a datagram
/// socket.
pub(crate) fn unbound_socket(domain: i32, ty: i32, protocol: i32) -> Option<UnboundSocket> {
    if domain != libc::AF_INET && domain != libc::AF_INET6 {
        return None;
    }
    let transport = match (ty & SOCK_TYPE_MASK, protocol) {
        (libc::SOCK_STREAM, 0 | libc::IPPROTO_TCP) => PortTransport::Tcp,
        (libc::SOCK_DGRAM, 0 | libc::IPPROTO_UDP) => PortTransport::Udp,
        _ => return None,
    };
    Some(UnboundSocket {
        transport,
        family: domain as u16,
    })
}

/// A call that can bind a socket without a local port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImplicitBindCall {
    /// `connect(2)` to an address of this family and length.
    Connect {
        /// The address's family.
        family: u16,
        /// The address length the guest passed.
        length: usize,
    },
    /// `listen(2)`.
    Listen,
    /// `sendto(2)`, `sendmsg(2)`, or `sendmmsg(2)` with a message to send.
    Send,
}

impl ImplicitBindCall {
    /// The `connect(2)` of the address the guest passed, or `None` when Linux
    /// fails the call before it reaches the socket: an address it cannot copy
    /// in (`move_addr_to_kernel`) or shorter than a family.
    pub(crate) fn connect<M: MemoryAccess>(
        memory: &M,
        address: Option<AddrMut<'_, libc::sockaddr>>,
        length: i32,
    ) -> Option<Self> {
        let bytes = read_sockaddr(memory, address, length).ok()?;
        let family = bytes.get(..2)?;
        Some(Self::Connect {
            family: u16::from_ne_bytes([family[0], family[1]]),
            length: bytes.len(),
        })
    }
}

/// Whether `call` on the unbound `socket` makes Linux pick it a port: the
/// errno the call fails with when no port is free, or `None` when Linux
/// finishes or fails the call without a port.
fn picks_a_port(socket: UnboundSocket, call: ImplicitBindCall) -> Option<Errno> {
    match (socket.transport, call) {
        // inet_dgram_connect binds before UDP checks the address; only
        // AF_UNSPEC, a disconnect, binds nothing (inet_autobind: EAGAIN).
        (PortTransport::Udp, ImplicitBindCall::Connect { family, .. }) => {
            (family != libc::AF_UNSPEC as u16).then_some(Errno::EAGAIN)
        }
        // inet_send_prepare binds before udp_sendmsg looks at the message.
        (PortTransport::Udp, ImplicitBindCall::Send) => Some(Errno::EAGAIN),
        // tcp_v4_connect and tcp_v6_connect check the address's length and
        // family before __inet_hash_connect picks the port (EADDRNOTAVAIL).
        (PortTransport::Tcp, ImplicitBindCall::Connect { family, length }) => {
            let shortest = match i32::from(socket.family) {
                libc::AF_INET => size_of::<libc::sockaddr_in>(),
                _ => SOCKADDR_IN6_RFC2133_LEN,
            };
            (family == socket.family && length >= shortest).then_some(Errno::EADDRNOTAVAIL)
        }
        // inet_csk_listen_start binds an unbound socket (EADDRINUSE).
        (PortTransport::Tcp, ImplicitBindCall::Listen) => Some(Errno::EADDRINUSE),
        (PortTransport::Udp, ImplicitBindCall::Listen)
        | (PortTransport::Tcp, ImplicitBindCall::Send) => None,
    }
}

/// The local address of a socket, as `getsockname(2)` returned it.
struct LocalAddress {
    bytes: Vec<u8>,
}

impl LocalAddress {
    /// The address's port, at the same offset in `sockaddr_in` and
    /// `sockaddr_in6`, in network byte order.
    fn port(&self) -> u16 {
        u16::from_be_bytes([self.bytes[2], self.bytes[3]])
    }

    /// The address with `port` in place of its own.
    fn with_port(&self, port: u16) -> Vec<u8> {
        let mut bytes = self.bytes.clone();
        bytes[2..4].copy_from_slice(&port.to_be_bytes());
        bytes
    }

    /// An IPv4 or IPv6 address, or `None` for a family without ports.
    fn parse(bytes: Vec<u8>) -> Option<Self> {
        let family = i32::from(u16::from_ne_bytes(bytes.get(..2)?.try_into().ok()?));
        let length = match family {
            libc::AF_INET => size_of::<libc::sockaddr_in>(),
            libc::AF_INET6 => size_of::<libc::sockaddr_in6>(),
            _ => return None,
        };
        (bytes.len() == length).then_some(Self { bytes })
    }
}

/// Where `bind_implicit_port` stages its calls: a transient private page,
/// mapped for one implicit bind and unmapped before the guest's call runs.
/// Linux reads the guest's own arguments after these calls, so the page
/// shares no byte with them, and the guest never learns its address.
#[derive(Clone, Copy)]
struct Scratch {
    page: AddrMut<'static, u8>,
}

impl Scratch {
    /// The page's length: one `sockaddr_in6` and the lengths and value the
    /// calls write back.
    const LENGTH: usize = 64;
    const ADDRESS: usize = 0;
    const ADDRESS_LENGTH: usize = 32;
    const RANGE: usize = 40;
    const RANGE_LENGTH: usize = 48;

    fn at<T>(self, offset: usize) -> AddrMut<'static, T> {
        AddrMut::from_raw(self.page.as_raw() + offset).expect("the page is mapped")
    }

    /// `getsockname(fd)`, or `None` when the descriptor has no IPv4 or IPv6
    /// address or a call fails.
    async fn local_address<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
        fd: RawFd,
    ) -> Option<LocalAddress> {
        let length = self.at::<libc::socklen_t>(Self::ADDRESS_LENGTH);
        let capacity = size_of::<libc::sockaddr_in6>();
        guest
            .memory()
            .write_value(length, &(capacity as libc::socklen_t))
            .ok()?;
        let call = syscalls::Getsockname::new()
            .with_fd(fd)
            .with_usockaddr(Some(self.at(Self::ADDRESS)))
            .with_usockaddr_len(Some(length));
        guest.inject_with_retry(call).await.ok()?;
        let returned: libc::socklen_t = guest.memory().read_value(length).ok()?;
        let mut bytes = vec![0; (returned as usize).min(capacity)];
        guest
            .memory()
            .read_exact(self.at::<u8>(Self::ADDRESS), &mut bytes)
            .ok()?;
        LocalAddress::parse(bytes)
    }

    /// The socket's `IP_LOCAL_PORT_RANGE`, 0 when it is unset or the kernel
    /// has no such option.
    async fn port_range<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
        fd: RawFd,
    ) -> u32 {
        let value = self.at::<u32>(Self::RANGE);
        let length = self.at::<libc::socklen_t>(Self::RANGE_LENGTH);
        if guest
            .memory()
            .write_value(length, &(size_of::<u32>() as libc::socklen_t))
            .is_err()
        {
            return 0;
        }
        let call = syscalls::Getsockopt::new()
            .with_fd(fd)
            .with_level(libc::SOL_IP)
            .with_optname(IP_LOCAL_PORT_RANGE)
            .with_optval(Some(value.cast()))
            .with_optlen(Some(length));
        match guest.inject_with_retry(call).await {
            Ok(_) => guest.memory().read_value(value).unwrap_or(0),
            Err(_) => 0,
        }
    }

    /// `bind(fd)` to `address`.
    async fn bind<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
        fd: RawFd,
        address: &[u8],
    ) -> Result<(), Errno> {
        guest
            .memory()
            .write_exact(self.at::<u8>(Self::ADDRESS), address)?;
        let call = syscalls::Bind::new()
            .with_fd(fd)
            .with_umyaddr(Some(self.at(Self::ADDRESS)))
            .with_addrlen(address.len() as i32);
        guest.inject_with_retry(call).await.map(drop)
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Before `call` on `fd`, bind the socket to a port from Detcore's
    /// allocator if `call` would make Linux pick one (see this module). `Err`
    /// is the errno Linux fails `call` with when no port is free; the caller
    /// then returns it without forwarding `call`. Detcore's own failures
    /// never fail `call`: Linux then picks the port.
    pub(crate) async fn bind_implicit_port<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        call: ImplicitBindCall,
    ) -> Result<(), Errno> {
        if self.cfg.recordreplay_modes {
            return Ok(());
        }
        let Ok(Some((socket, open_file))) = guest.thread_state().with_detfd(fd, |detfd| {
            detfd
                .unbound_socket()
                .map(|socket| (socket, detfd.open_file_id()))
        }) else {
            return Ok(());
        };
        let Some(exhausted) = picks_a_port(socket, call) else {
            return Ok(());
        };
        let Some(page) = map_transient(guest, Scratch::LENGTH).await else {
            warn!("[fd {fd}] no page to stage an implicit bind; Linux picks the port");
            return Ok(());
        };
        let bound = self
            .bind_in_scratch(guest, fd, open_file, exhausted, Scratch { page })
            .await;
        unmap_transient(guest, page, Scratch::LENGTH).await;
        bound
    }

    /// [`Self::bind_implicit_port`] with its calls staged in `scratch`.
    async fn bind_in_scratch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        open_file: OpenFileId,
        exhausted: Errno,
        scratch: Scratch,
    ) -> Result<(), Errno> {
        let Some(local) = scratch.local_address(guest, fd).await else {
            // The guest's own call reports what is wrong with the descriptor.
            return Ok(());
        };
        if local.port() != 0 {
            // Bound by a call Detcore did not see bind it: count its port.
            send_and_update_time(guest, GlobalRequest::AddUsedPort(local.port(), open_file)).await;
            mark_bound(guest, fd);
            return Ok(());
        }
        let socket_range = scratch.port_range(guest, fd).await;
        let mut refused = false;
        loop {
            let request = GlobalRequest::ImplicitPort {
                open_file,
                refused,
                socket_range,
            };
            let port = match send_and_update_time(guest, request).await.1 {
                GlobalResponse::RequestPort(port) => port,
                GlobalResponse::PortFull => return Err(exhausted),
                other => panic!("unexpected implicit-port response: {other:?}"),
            };
            match scratch.bind(guest, fd, &local.with_port(port)).await {
                Ok(()) => {
                    trace!("[fd {fd}] implicit bind to port {port}");
                    mark_bound(guest, fd);
                    return Ok(());
                }
                Err(Errno::EADDRINUSE) => refused = true,
                Err(errno) => {
                    // The port stays counted for this socket until it closes.
                    warn!(
                        "[fd {fd}] binding port {port} failed with {errno}; Linux picks the port"
                    );
                    return Ok(());
                }
            }
        }
    }
}

/// Record the socket `call` just created on `fd` as one without a local port,
/// if it is TCP or UDP.
pub(crate) fn mark_unbound_port<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &mut G,
    fd: RawFd,
    call: &syscalls::Socket,
) {
    let Some(socket) = unbound_socket(call.family(), call.r#type(), call.protocol()) else {
        return;
    };
    let _ = guest
        .thread_state()
        .with_detfd(fd, |detfd| detfd.set_unbound_socket(Some(socket)));
}

/// Record that the socket on `fd` has a local port, so later calls on it
/// skip the check.
pub(crate) fn mark_bound<T: RecordOrReplay, G: Guest<Detcore<T>>>(guest: &mut G, fd: RawFd) {
    let _ = guest
        .thread_state()
        .with_detfd(fd, |detfd| detfd.set_unbound_socket(None));
}

#[cfg(test)]
mod tests {
    use super::*;

    const INET: u16 = libc::AF_INET as u16;
    const INET6: u16 = libc::AF_INET6 as u16;
    const UNSPEC: u16 = libc::AF_UNSPEC as u16;

    fn connect(family: u16, length: usize) -> ImplicitBindCall {
        ImplicitBindCall::Connect { family, length }
    }

    fn socket(transport: PortTransport, family: u16) -> UnboundSocket {
        UnboundSocket { transport, family }
    }

    #[test]
    fn tcp_and_udp_sockets_over_ipv4_and_ipv6_have_ports_linux_picks() {
        use PortTransport::*;
        for domain in [libc::AF_INET, libc::AF_INET6] {
            let family = domain as u16;
            for flags in [0, libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC] {
                let stream = libc::SOCK_STREAM | flags;
                let datagram = libc::SOCK_DGRAM | flags;
                let tcp = Some(socket(Tcp, family));
                let udp = Some(socket(Udp, family));
                assert_eq!(unbound_socket(domain, stream, 0), tcp);
                assert_eq!(unbound_socket(domain, stream, libc::IPPROTO_TCP), tcp);
                assert_eq!(unbound_socket(domain, datagram, 0), udp);
                assert_eq!(unbound_socket(domain, datagram, libc::IPPROTO_UDP), udp);
            }
            assert_eq!(unbound_socket(domain, libc::SOCK_STREAM, 262), None); // MPTCP
            assert_eq!(
                unbound_socket(domain, libc::SOCK_DGRAM, libc::IPPROTO_ICMP),
                None
            );
            assert_eq!(unbound_socket(domain, libc::SOCK_RAW, 0), None);
        }
        assert_eq!(unbound_socket(libc::AF_UNIX, libc::SOCK_STREAM, 0), None);
        assert_eq!(unbound_socket(libc::AF_NETLINK, libc::SOCK_DGRAM, 0), None);
    }

    #[test]
    fn udp_picks_a_port_at_every_connect_but_a_disconnect_and_at_every_send() {
        let eagain = Some(Errno::EAGAIN);
        for family in [INET, INET6] {
            let udp = socket(PortTransport::Udp, family);
            assert_eq!(picks_a_port(udp, connect(INET, 16)), eagain);
            assert_eq!(picks_a_port(udp, connect(INET6, 2)), eagain);
            assert_eq!(picks_a_port(udp, connect(UNSPEC, 16)), None);
            assert_eq!(picks_a_port(udp, ImplicitBindCall::Send), eagain);
            assert_eq!(picks_a_port(udp, ImplicitBindCall::Listen), None);
        }
    }

    #[test]
    fn tcp_picks_a_port_at_a_connect_to_an_address_of_its_family_and_at_listen() {
        let unavailable = Some(Errno::EADDRNOTAVAIL);
        let tcp4 = socket(PortTransport::Tcp, INET);
        let tcp6 = socket(PortTransport::Tcp, INET6);
        assert_eq!(picks_a_port(tcp4, connect(INET, 16)), unavailable);
        assert_eq!(picks_a_port(tcp4, connect(INET, 15)), None);
        assert_eq!(picks_a_port(tcp4, connect(INET6, 28)), None);
        assert_eq!(picks_a_port(tcp4, connect(UNSPEC, 16)), None);
        assert_eq!(picks_a_port(tcp6, connect(INET6, 24)), unavailable);
        assert_eq!(picks_a_port(tcp6, connect(INET6, 23)), None);
        assert_eq!(picks_a_port(tcp6, connect(INET, 16)), None);
        for tcp in [tcp4, tcp6] {
            assert_eq!(
                picks_a_port(tcp, ImplicitBindCall::Listen),
                Some(Errno::EADDRINUSE)
            );
            assert_eq!(picks_a_port(tcp, ImplicitBindCall::Send), None);
        }
    }

    #[test]
    fn only_a_whole_ipv4_or_ipv6_address_is_a_local_address() {
        let mut inet = vec![0; size_of::<libc::sockaddr_in>()];
        inet[..2].copy_from_slice(&INET.to_ne_bytes());
        inet[2..4].copy_from_slice(&32768u16.to_be_bytes());
        let local = LocalAddress::parse(inet.clone()).expect("an IPv4 address");
        assert_eq!(local.port(), 32768);
        assert_eq!(local.with_port(40000)[2..4], 40000u16.to_be_bytes());
        assert_eq!(local.with_port(40000)[4..], inet[4..]);
        let mut inet6 = vec![0; size_of::<libc::sockaddr_in6>()];
        inet6[..2].copy_from_slice(&INET6.to_ne_bytes());
        assert_eq!(
            LocalAddress::parse(inet6).map(|local| local.port()),
            Some(0)
        );
        assert!(LocalAddress::parse(inet[..8].to_vec()).is_none());
        let unix = (libc::AF_UNIX as u16).to_ne_bytes().to_vec();
        assert!(LocalAddress::parse(unix).is_none());
        assert!(LocalAddress::parse(Vec::new()).is_none());
    }
}
