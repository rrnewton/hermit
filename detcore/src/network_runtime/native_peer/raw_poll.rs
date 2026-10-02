//! One actual nonconsuming poll on the retained original OFD.
use detcore_model::network_trace::NETWORK_TCP_POLL_REQUEST;
use detcore_model::network_trace::valid_tcp_poll_mask;

use super::*;

pub(super) fn observe(fd: BorrowedFd<'_>) -> Observation {
    let mut row = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: NETWORK_TCP_POLL_REQUEST,
        revents: 0,
    };
    let raw = unsafe { libc::poll(&raw mut row, 1, 0) };
    observation(i64::from(raw), Vec::new(), |_| ResultValue::PollState {
        revents: row.revents,
    })
}

pub(super) fn validate(observed: &Observation) -> io::Result<()> {
    let valid = match observed.confirmation {
        ResultValue::PollState { revents } => {
            valid_tcp_poll_mask(revents)
                && observed.raw_return == i64::from(revents != 0)
                && observed.errno.is_none()
        }
        ResultValue::Errno(errno) => {
            (1..=4095).contains(&errno)
                && observed.raw_return == -1
                && observed.errno == Some(errno)
        }
        _ => false,
    };
    if !valid || !observed.bytes.is_empty() || observed.helper_copy.is_some() {
        return Err(io::Error::other(
            "native poll raw return/mask differs from retained result",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_poll_retained_tcp_observes_aliases_eof_and_never_consumes_payload() {
        let (original, peer) = super::super::tests::pair();
        let before = observe(original.as_fd());
        validate(&before).unwrap();
        let ResultValue::PollState { revents } = before.confirmation else {
            panic!("poll state")
        };
        assert_eq!(
            revents & (libc::POLLOUT | libc::POLLWRNORM),
            libc::POLLOUT | libc::POLLWRNORM
        );
        assert_eq!(
            revents & (libc::POLLIN | libc::POLLRDNORM | libc::POLLPRI),
            0
        );
        assert_eq!(
            unsafe {
                libc::send(
                    peer.as_raw_fd(),
                    b"abc".as_ptr().cast(),
                    3,
                    libc::MSG_NOSIGNAL,
                )
            },
            3
        );
        let mut wait = libc::pollfd {
            fd: original.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&raw mut wait, 1, 1000) }, 1);
        let data = observe(original.as_fd());
        validate(&data).unwrap();
        let ResultValue::PollState { revents } = data.confirmation else {
            panic!("poll state")
        };
        assert_eq!(
            revents & (libc::POLLIN | libc::POLLRDNORM),
            libc::POLLIN | libc::POLLRDNORM
        );
        let mut bytes = [0u8; 3];
        assert_eq!(
            unsafe {
                libc::recv(
                    original.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    3,
                    libc::MSG_DONTWAIT,
                )
            },
            3
        );
        assert_eq!(&bytes, b"abc");
        assert_eq!(
            unsafe {
                libc::send(
                    peer.as_raw_fd(),
                    b"!".as_ptr().cast(),
                    1,
                    libc::MSG_NOSIGNAL | libc::MSG_OOB,
                )
            },
            1
        );
        wait.events = libc::POLLPRI;
        assert_eq!(unsafe { libc::poll(&raw mut wait, 1, 1000) }, 1);
        let urgent = observe(original.as_fd());
        validate(&urgent).unwrap();
        let ResultValue::PollState { revents } = urgent.confirmation else {
            panic!("poll state")
        };
        assert_eq!(revents & libc::POLLPRI, libc::POLLPRI);
        assert_eq!(
            unsafe {
                libc::recv(
                    original.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    1,
                    libc::MSG_DONTWAIT | libc::MSG_OOB,
                )
            },
            1
        );
        assert_eq!(bytes[0], b'!');
        let cleared = observe(original.as_fd());
        validate(&cleared).unwrap();
        let ResultValue::PollState { revents } = cleared.confirmation else {
            panic!("poll state")
        };
        assert_eq!(revents & libc::POLLPRI, 0);
        assert_eq!(
            unsafe { libc::shutdown(peer.as_raw_fd(), libc::SHUT_WR) },
            0
        );
        wait.events = libc::POLLIN;
        assert_eq!(unsafe { libc::poll(&raw mut wait, 1, 1000) }, 1);
        let eof = observe(original.as_fd());
        validate(&eof).unwrap();
        let ResultValue::PollState { revents } = eof.confirmation else {
            panic!("poll state")
        };
        assert_eq!(
            revents & (libc::POLLIN | libc::POLLRDNORM | libc::POLLRDHUP),
            libc::POLLIN | libc::POLLRDNORM | libc::POLLRDHUP
        );
        assert_eq!(
            unsafe {
                libc::recv(
                    original.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    3,
                    libc::MSG_DONTWAIT,
                )
            },
            0
        );
    }

    #[test]
    fn raw_poll_confirmation_refuses_count_errno_bytes_unknown_mask_and_nval() {
        let valid = Observation {
            raw_return: 0,
            errno: None,
            bytes: vec![],
            confirmation: ResultValue::PollState { revents: 0 },
            helper_copy: None,
        };
        validate(&valid).unwrap();
        for index in 0..6 {
            let mut bad = valid.clone();
            match index {
                0 => bad.raw_return = 1,
                1 => bad.errno = Some(libc::EINTR),
                2 => bad.bytes.push(7),
                3 => {
                    bad.confirmation = ResultValue::PollState {
                        revents: libc::POLLNVAL,
                    }
                }
                4 => bad.confirmation = ResultValue::PollState { revents: i16::MIN },
                _ => {
                    bad.confirmation = ResultValue::PollState {
                        revents: libc::POLLOUT,
                    }
                }
            }
            assert!(validate(&bad).is_err(), "case {index}");
        }
        let mut terminal = valid;
        terminal.raw_return = 1;
        terminal.confirmation = ResultValue::PollState {
            revents: libc::POLLERR | libc::POLLHUP | libc::POLLPRI,
        };
        validate(&terminal).unwrap();
        terminal.raw_return = 2;
        assert!(validate(&terminal).is_err());
    }
}
