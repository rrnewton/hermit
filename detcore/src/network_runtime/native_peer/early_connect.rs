//! Non-consuming completion observation on the original retained Connect pin.
//!
//! This is deliberately not a pending-handshake waiter. A real EINPROGRESS
//! result remains EINPROGRESS; only an already established, nonblocking TCP
//! socket can supply the separate V4 establishment input.

use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;

use detcore_model::network_trace::NetworkAddressV2;

#[derive(Debug, Clone)]
pub(super) struct Completion {
    peer: NetworkAddressV2,
}

impl Completion {
    pub(super) fn observe(fd: BorrowedFd<'_>) -> io::Result<Self> {
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of_val(&address) as libc::socklen_t;
        if unsafe {
            libc::getpeername(
                fd.as_raw_fd(),
                std::ptr::from_mut(&mut address).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let peer = match i32::from(address.ss_family) {
            libc::AF_INET if length as usize == std::mem::size_of::<libc::sockaddr_in>() => {
                let address = unsafe { &*std::ptr::from_ref(&address).cast::<libc::sockaddr_in>() };
                NetworkAddressV2::Inet4 {
                    address: address.sin_addr.s_addr.to_ne_bytes(),
                    port: u16::from_be(address.sin_port),
                }
            }
            libc::AF_INET6 if length as usize == std::mem::size_of::<libc::sockaddr_in6>() => {
                let address =
                    unsafe { &*std::ptr::from_ref(&address).cast::<libc::sockaddr_in6>() };
                NetworkAddressV2::Inet6 {
                    address: address.sin6_addr.s6_addr,
                    port: u16::from_be(address.sin6_port),
                    flowinfo: address.sin6_flowinfo,
                    scope_id: address.sin6_scope_id,
                }
            }
            _ => {
                return Err(io::Error::other(
                    "early Connect peer has unsupported family/length",
                ));
            }
        };
        // TCP_INFO's first byte is tcpi_state. Unlike SO_ERROR this query does
        // not consume a pending error. Check after getpeername so CLOSE_WAIT,
        // SYN_SENT and failed connections cannot be promoted by a peer alone.
        let mut state = 0u8;
        let mut state_length = 1;
        if unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_INFO,
                std::ptr::from_mut(&mut state).cast(),
                &mut state_length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if state_length != 1 {
            return Err(io::Error::other(
                "early Connect TCP_INFO state length changed",
            ));
        }
        check_state(flags, state)?;
        Ok(Self { peer })
    }

    pub(super) fn confirm(&self, copied_peer: &NetworkAddressV2) -> io::Result<()> {
        if &self.peer != copied_peer {
            return Err(io::Error::other(
                "early Connect peer differs from original copied target",
            ));
        }
        Ok(())
    }
}

fn check_state(flags: i32, tcp_state: u8) -> io::Result<()> {
    // Linux TCP_ESTABLISHED is 1. In particular, SO_ERROR == 0 does not imply
    // this state: it also occurs while a nonblocking connect is still pending.
    if flags & libc::O_NONBLOCK == 0 || tcp_state != 1 {
        return Err(io::Error::other(
            "early Connect is not nonblocking and established",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;

    use super::*;

    #[test]
    fn early_connect_requires_established_not_zero_socket_error() {
        let raw =
            unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, 0) };
        assert!(raw >= 0);
        let socket = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut error = -1i32;
        let mut length = std::mem::size_of_val(&error) as libc::socklen_t;
        let queried = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                std::ptr::from_mut(&mut error).cast(),
                &mut length,
            )
        };
        let observed = Completion::observe(socket.as_fd());
        drop(socket);
        assert_eq!(queried, 0);
        assert_eq!(error, 0);
        assert!(observed.is_err());
        assert!(check_state(libc::O_NONBLOCK, 2).is_err()); // SYN_SENT
        assert!(check_state(libc::O_NONBLOCK, 7).is_err()); // CLOSE
        assert!(check_state(libc::O_NONBLOCK, 8).is_err()); // CLOSE_WAIT
        assert!(check_state(0, 1).is_err());
        assert!(check_state(libc::O_NONBLOCK, 1).is_ok());
    }

    #[test]
    fn early_connect_observes_real_pin_and_rejects_different_peer() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let socket = std::net::TcpStream::connect(address).unwrap();
        socket.set_nonblocking(true).unwrap();
        let observed = Completion::observe(socket.as_fd());
        drop(socket);
        drop(listener);
        let observed = observed.unwrap();
        let peer = NetworkAddressV2::Inet4 {
            address: [127, 0, 0, 1],
            port: address.port(),
        };
        assert!(observed.confirm(&peer).is_ok());
        assert!(
            observed
                .confirm(&NetworkAddressV2::Inet4 {
                    address: [127, 0, 0, 2],
                    port: address.port(),
                })
                .is_err()
        );
    }
}
