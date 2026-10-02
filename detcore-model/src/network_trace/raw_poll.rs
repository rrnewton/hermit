//! Exact TCP poll observations. These bits are not receive-copy authority.

/// Request every TCP readiness class, including exceptional and normal aliases.
/// Linux adds ERR/HUP independently of this request. NVAL on an owned pin is a
/// custody failure, not a portable readiness state.
pub const NETWORK_TCP_POLL_REQUEST: i16 = libc::POLLIN
    | libc::POLLOUT
    | libc::POLLPRI
    | libc::POLLRDNORM
    | libc::POLLWRNORM
    | libc::POLLRDBAND
    | libc::POLLWRBAND
    | libc::POLLRDHUP;

/// Whether a completed poll on a retained TCP file has a representable mask.
pub fn valid_tcp_poll_mask(revents: i16) -> bool {
    revents & !(NETWORK_TCP_POLL_REQUEST | libc::POLLERR | libc::POLLHUP) == 0
}

/// Apply Linux do_pollfd's per-row filtering to one complete raw observation.
/// This deliberately does not turn PRI, RDHUP or normal aliases into IN/OUT.
pub fn tcp_poll_row_mask(revents: i16, events: i16) -> i16 {
    revents & (events | libc::POLLERR | libc::POLLHUP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_poll_masks_preserve_aliases_exceptional_terminal_and_zero() {
        for mask in [
            0,
            libc::POLLIN,
            libc::POLLOUT,
            libc::POLLPRI,
            libc::POLLRDNORM,
            libc::POLLWRNORM,
            libc::POLLRDHUP,
            libc::POLLERR,
            libc::POLLHUP,
            NETWORK_TCP_POLL_REQUEST,
        ] {
            assert!(valid_tcp_poll_mask(mask));
        }
        assert!(!valid_tcp_poll_mask(libc::POLLNVAL));
        assert!(!valid_tcp_poll_mask(i16::MIN));
        let raw = libc::POLLPRI
            | libc::POLLRDHUP
            | libc::POLLRDNORM
            | libc::POLLWRNORM
            | libc::POLLERR
            | libc::POLLHUP;
        assert_eq!(
            tcp_poll_row_mask(raw, libc::POLLIN | libc::POLLOUT),
            libc::POLLERR | libc::POLLHUP
        );
        assert_eq!(tcp_poll_row_mask(raw, 0), libc::POLLERR | libc::POLLHUP);
        assert_eq!(tcp_poll_row_mask(raw, NETWORK_TCP_POLL_REQUEST), raw);
        assert_eq!(tcp_poll_row_mask(0, NETWORK_TCP_POLL_REQUEST), 0);
    }
}
