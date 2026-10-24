/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore's allocator for the local ports Linux would otherwise choose at
//! random.
//!
//! One pool serves a bind to port 0 (`Detcore::handle_bind`), an implicit
//! bind (`crate::syscalls::implicit_bind`), and the `AF_UNIX` and Netlink
//! autobinds, whose names take a number from it. A port stays given out
//! until the open file description holding it closes. Every request draws
//! from an inclusive range its caller reads when it asks, skipping the
//! reserved ports, as each Linux pick does: the network namespace's
//! `ip_local_port_range`, narrowed for an implicit bind by the socket's
//! `IP_LOCAL_PORT_RANGE` (`inet_sk_get_local_port_range`), and
//! `ip_local_reserved_ports` (`inet_is_local_reserved_port`). The pool is
//! one space for TCP and UDP alike, where Linux keeps one per protocol, so a
//! range both fill runs out sooner here.

use std::collections::HashMap;
use std::ops::RangeInclusive;

use crate::types::OpenFileId;

/// The ports Detcore has given out, and the open file description holding
/// each.
#[derive(Debug, Default)]
pub(crate) struct PortAllocator {
    /// Where the next bind to port 0 searches from. Outside the range, a
    /// search starts at the range's low end.
    bind_cursor: u16,
    /// Where the next implicit bind searches from.
    implicit_cursor: u16,
    /// Every port given out and not yet released, with the number of holders
    /// counting it: a guest may bind one port twice (TCP and UDP, or
    /// `SO_REUSEPORT`), and each close releases one.
    used: HashMap<u16, usize>,
    /// The port each open file description holds.
    holders: HashMap<OpenFileId, u16>,
    /// Ports the kernel refused for each socket's implicit bind, which stay
    /// out of the pool until that socket closes.
    refused: HashMap<OpenFileId, Vec<u16>>,
}

impl PortAllocator {
    /// A free port in `range` for a bind to port 0. Its cursor stays on the
    /// port, so it is offered again first once released; implicit binds
    /// search on past it.
    pub(crate) fn request(
        &mut self,
        holder: OpenFileId,
        range: (u16, u16),
        reserved: &[RangeInclusive<u16>],
    ) -> Option<u16> {
        let port = self.first_free_port(self.bind_cursor, range, reserved)?;
        self.bind_cursor = port;
        if port >= self.implicit_cursor {
            self.implicit_cursor = after(port, range);
        }
        self.hold(holder, port);
        Some(port)
    }

    /// A free port in `range` for an implicit bind of `holder`, a socket the
    /// kernel reports unbound. The cursor moves past the port, as Linux's
    /// `connect` moves on from the port it took, so a port is offered again
    /// only after the range wraps: until then, the state a closed connection
    /// leaves on it in the kernel (`TIME_WAIT`, a final `ACK` in flight)
    /// cannot decide whether a bind to it succeeds.
    ///
    /// With `refused`, the kernel refused the port `holder` was given last,
    /// because a socket this pool does not count holds it; that port stays
    /// out of the pool until `holder` closes. Otherwise a port recorded for
    /// `holder` is not its own (a bind of it failed) and goes back.
    pub(crate) fn request_implicit(
        &mut self,
        holder: OpenFileId,
        range: (u16, u16),
        reserved: &[RangeInclusive<u16>],
        refused: bool,
    ) -> Option<u16> {
        if let Some(previous) = self.holders.remove(&holder) {
            if refused {
                self.refused.entry(holder).or_default().push(previous);
            } else {
                self.uncount(previous);
            }
        }
        let port = self.first_free_port(self.implicit_cursor, range, reserved)?;
        self.implicit_cursor = after(port, range);
        self.hold(holder, port);
        Some(port)
    }

    /// Count `port`, which `holder` bound itself, as given out.
    pub(crate) fn add_used(&mut self, holder: OpenFileId, port: u16) {
        self.hold(holder, port);
    }

    /// Return the port `holder` holds, and the ports the kernel refused it,
    /// to the pool.
    pub(crate) fn release(&mut self, holder: OpenFileId) -> Option<u16> {
        for port in self.refused.remove(&holder).unwrap_or_default() {
            self.uncount(port);
        }
        let port = self.holders.remove(&holder)?;
        self.uncount(port);
        Some(port)
    }

    /// Record that `holder` holds `port`. A port it held before stays
    /// counted, as an earlier bind of the same socket may still hold it.
    fn hold(&mut self, holder: OpenFileId, port: u16) {
        if self.holders.insert(holder, port) != Some(port) {
            *self.used.entry(port).or_default() += 1;
        }
    }

    fn uncount(&mut self, port: u16) {
        if let Some(holders) = self.used.get_mut(&port) {
            *holders -= 1;
            if *holders == 0 {
                self.used.remove(&port);
            }
        }
    }

    /// The first port of the inclusive `range` that is neither given out nor
    /// reserved, searching up from `cursor` and wrapping round; a cursor
    /// outside the range starts at its low end.
    fn first_free_port(
        &self,
        cursor: u16,
        (low, high): (u16, u16),
        reserved: &[RangeInclusive<u16>],
    ) -> Option<u16> {
        let start = if (low..=high).contains(&cursor) {
            cursor
        } else {
            low
        };
        (start..=high).chain(low..start).find(|port| {
            !self.used.contains_key(port) && !reserved.iter().any(|ports| ports.contains(port))
        })
    }
}

/// The port after `port`, which a search from it leaves for the range's low
/// end once past the high end.
fn after(port: u16, range: (u16, u16)) -> u16 {
    port.checked_add(1).unwrap_or(range.0)
}

/// The range Linux picks a socket's port from: the namespace's `range`,
/// narrowed by the socket's `IP_LOCAL_PORT_RANGE` (`socket`: low port in the
/// low 16 bits, high port in the high 16 bits, 0 when unset) wherever an end
/// lies inside it, as `inet_sk_get_local_port_range` does.
pub(crate) fn effective_port_range((mut low, mut high): (u16, u16), socket: u32) -> (u16, u16) {
    let (socket_low, socket_high) = (socket as u16, (socket >> 16) as u16);
    if socket != 0 {
        if (low..=high).contains(&socket_low) {
            low = socket_low;
        }
        if (low..=high).contains(&socket_high) {
            high = socket_high;
        }
    }
    (low, high)
}

/// The ports `/proc/sys/net/ipv4/ip_local_reserved_ports` lists, a
/// comma-separated list of ports and inclusive ranges (`8000-8010,9000`).
/// Anything else is skipped.
pub(crate) fn parse_reserved_ports(contents: &str) -> Vec<RangeInclusive<u16>> {
    contents
        .trim()
        .split(',')
        .filter_map(|entry| {
            let (low, high) = entry.split_once('-').unwrap_or((entry, entry));
            Some(low.trim().parse().ok()?..=high.trim().parse().ok()?)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DetTid;

    const RANGE: (u16, u16) = (32768, 60999);

    fn socket(sequence: u64) -> OpenFileId {
        OpenFileId::new_socket(DetTid::from_raw(3), sequence)
    }

    #[test]
    fn a_bind_to_port_zero_takes_the_low_end_first_and_then_the_next_port() {
        let mut ports = PortAllocator::default();
        assert_eq!(ports.request(socket(1), RANGE, &[]), Some(32768));
        assert_eq!(ports.request(socket(2), RANGE, &[]), Some(32769));
    }

    #[test]
    fn a_released_port_is_offered_again_to_the_next_bind_to_port_zero() {
        let mut ports = PortAllocator::default();
        assert_eq!(ports.request(socket(1), RANGE, &[]), Some(32768));
        assert_eq!(ports.release(socket(1)), Some(32768));
        assert_eq!(ports.request(socket(2), RANGE, &[]), Some(32768));
        assert_eq!(ports.release(socket(3)), None);
    }

    #[test]
    fn an_implicit_bind_moves_past_its_port_so_a_released_port_waits_for_the_wrap() {
        let mut ports = PortAllocator::default();
        let range = (40000, 40002);
        assert_eq!(
            ports.request_implicit(socket(1), range, &[], false),
            Some(40000)
        );
        assert_eq!(ports.release(socket(1)), Some(40000));
        assert_eq!(
            ports.request_implicit(socket(2), range, &[], false),
            Some(40001)
        );
        assert_eq!(
            ports.request_implicit(socket(3), range, &[], false),
            Some(40002)
        );
        assert_eq!(
            ports.request_implicit(socket(4), range, &[], false),
            Some(40000)
        );
    }

    #[test]
    fn a_port_a_bind_to_port_zero_released_waits_for_the_implicit_wrap() {
        let mut ports = PortAllocator::default();
        let range = (40000, 40003);
        assert_eq!(ports.request(socket(1), range, &[]), Some(40000));
        assert_eq!(ports.request(socket(2), range, &[]), Some(40001));
        assert_eq!(ports.release(socket(1)), Some(40000));
        assert_eq!(
            ports.request_implicit(socket(3), range, &[], false),
            Some(40002)
        );
        assert_eq!(
            ports.request_implicit(socket(4), range, &[], false),
            Some(40003)
        );
        assert_eq!(
            ports.request_implicit(socket(5), range, &[], false),
            Some(40000)
        );
    }

    #[test]
    fn a_refused_port_stays_out_of_the_pool_until_its_socket_closes() {
        let mut ports = PortAllocator::default();
        let range = (40000, 40001);
        assert_eq!(
            ports.request_implicit(socket(1), range, &[], false),
            Some(40000)
        );
        assert_eq!(
            ports.request_implicit(socket(1), range, &[], true),
            Some(40001)
        );
        assert_eq!(ports.request_implicit(socket(2), range, &[], false), None);
        assert_eq!(ports.release(socket(1)), Some(40001));
        assert_eq!(
            ports.request_implicit(socket(2), range, &[], false),
            Some(40000)
        );
    }

    #[test]
    fn a_socket_the_kernel_refused_every_port_finds_the_pool_full() {
        let mut ports = PortAllocator::default();
        let range = (40000, 40000);
        assert_eq!(
            ports.request_implicit(socket(1), range, &[], false),
            Some(40000)
        );
        assert_eq!(ports.request_implicit(socket(1), range, &[], true), None);
        assert_eq!(ports.release(socket(1)), None);
        assert_eq!(ports.request(socket(2), range, &[]), Some(40000));
    }

    #[test]
    fn a_port_recorded_by_a_failed_bind_goes_back_at_the_sockets_implicit_bind() {
        let mut ports = PortAllocator::default();
        // Socket 1 holds 40005; socket 2's bind of it failed, and was counted.
        ports.add_used(socket(1), 40005);
        ports.add_used(socket(2), 40005);
        assert_eq!(
            ports.request_implicit(socket(2), RANGE, &[], false),
            Some(32768)
        );
        assert_eq!(ports.request(socket(3), (40005, 40005), &[]), None);
        assert_eq!(ports.release(socket(1)), Some(40005));
        assert_eq!(ports.request(socket(3), (40005, 40005), &[]), Some(40005));
    }

    #[test]
    fn a_port_two_sockets_bound_stays_given_out_until_both_close() {
        let mut ports = PortAllocator::default();
        ports.add_used(socket(1), 40000);
        ports.add_used(socket(2), 40000);
        ports.add_used(socket(2), 40000);
        assert_eq!(ports.release(socket(1)), Some(40000));
        assert_eq!(ports.request(socket(3), (40000, 40000), &[]), None);
        assert_eq!(ports.release(socket(2)), Some(40000));
        assert_eq!(ports.request(socket(3), (40000, 40000), &[]), Some(40000));
    }

    #[test]
    fn every_port_of_a_range_is_offered_before_it_is_full() {
        let mut ports = PortAllocator::default();
        let range = (40000, 40002);
        ports.add_used(socket(1), 40001);
        assert_eq!(ports.request(socket(2), range, &[]), Some(40000));
        assert_eq!(ports.request(socket(3), range, &[]), Some(40002));
        assert_eq!(ports.request(socket(4), range, &[]), None);
        assert_eq!(ports.request(socket(5), (40000, 40000), &[]), None);
    }

    #[test]
    fn the_search_wraps_round_to_a_free_port_below_the_cursor() {
        let mut ports = PortAllocator::default();
        let range = (40000, 40002);
        assert_eq!(ports.request(socket(1), range, &[]), Some(40000));
        assert_eq!(ports.request(socket(2), range, &[]), Some(40001));
        assert_eq!(ports.release(socket(1)), Some(40000));
        assert_eq!(ports.request(socket(3), range, &[]), Some(40002));
        assert_eq!(ports.request(socket(4), range, &[]), Some(40000));
    }

    #[test]
    fn a_one_port_range_offers_its_port() {
        let mut ports = PortAllocator::default();
        assert_eq!(ports.request(socket(1), (40000, 40000), &[]), Some(40000));
        assert_eq!(
            ports.request_implicit(socket(2), (40001, 40001), &[], false),
            Some(40001)
        );
    }

    #[test]
    fn a_range_the_guest_moved_is_searched_from_its_low_end() {
        let mut ports = PortAllocator::default();
        assert_eq!(ports.request(socket(1), RANGE, &[]), Some(32768));
        assert_eq!(ports.request(socket(2), (40000, 40010), &[]), Some(40000));
        assert_eq!(
            ports.request_implicit(socket(3), (50000, 50010), &[], false),
            Some(50000)
        );
        assert_eq!(ports.request(socket(4), (40000, 40010), &[]), Some(40001));
    }

    #[test]
    fn the_search_wraps_round_from_the_high_end() {
        let mut ports = PortAllocator::default();
        let range = (65534, 65535);
        assert_eq!(
            ports.request_implicit(socket(1), range, &[], false),
            Some(65534)
        );
        assert_eq!(
            ports.request_implicit(socket(2), range, &[], false),
            Some(65535)
        );
        assert_eq!(ports.release(socket(1)), Some(65534));
        assert_eq!(
            ports.request_implicit(socket(3), range, &[], false),
            Some(65534)
        );
    }

    #[test]
    fn reserved_ports_are_never_offered() {
        let mut ports = PortAllocator::default();
        let reserved = parse_reserved_ports("40000-40001,40003\n");
        let range = (40000, 40004);
        assert_eq!(ports.request(socket(1), range, &reserved), Some(40002));
        assert_eq!(
            ports.request_implicit(socket(2), range, &reserved, false),
            Some(40004)
        );
        assert_eq!(ports.request(socket(3), range, &reserved), None);
    }

    #[test]
    fn reserved_ports_parse_as_linux_prints_them() {
        assert_eq!(parse_reserved_ports("\n"), Vec::new());
        assert_eq!(parse_reserved_ports("8080\n"), vec![8080..=8080]);
        assert_eq!(
            parse_reserved_ports("8000-8010,9000\n"),
            vec![8000..=8010, 9000..=9000]
        );
    }

    #[test]
    fn a_socket_range_narrows_the_namespace_range_where_its_ends_lie_inside() {
        let pack = |low: u32, high: u32| low | high << 16;
        assert_eq!(effective_port_range(RANGE, 0), RANGE);
        assert_eq!(
            effective_port_range(RANGE, pack(40000, 40010)),
            (40000, 40010)
        );
        assert_eq!(
            effective_port_range(RANGE, pack(1024, 40010)),
            (32768, 40010)
        );
        assert_eq!(
            effective_port_range(RANGE, pack(40000, 65535)),
            (40000, 60999)
        );
        assert_eq!(effective_port_range(RANGE, pack(0, 40010)), (32768, 40010));
    }
}
