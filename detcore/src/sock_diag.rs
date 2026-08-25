/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Determinize socket identities in `NETLINK_SOCK_DIAG` dump replies.
//!
//! Tools such as `ss` enumerate sockets over an `AF_NETLINK`/`NETLINK_SOCK_DIAG`
//! socket and receive a binary, multipart `nlmsghdr`-framed reply. Each
//! `SOCK_DIAG_BY_FAMILY` message carries a family-specific body whose socket
//! inode number (`udiag_ino` for `AF_UNIX`, `ndiag_ino` for `AF_NETLINK`) and
//! the two-word socket cookie are assigned by host-global kernel state and
//! therefore differ on every run.
//! `AF_UNIX` bodies additionally carry a `UNIX_DIAG_PEER` attribute holding the
//! peer socket's inode. Those numbers leak into guest-visible output (for
//! example `ss -a`), breaking `--strict --verify`.
//!
//! detcore already zeroes the same inodes in the procfs *text* interfaces
//! (`/proc/net/unix`, `/proc/net/netlink`; see `crate::procfs`). This module
//! removes those inodes and the binary-only socket cookies from the
//! `SOCK_DIAG` path so neither interface leaks host socket identities.
//!
//! The parser is deliberately pure and fail-open: on any framing inconsistency
//! it leaves the buffer untouched (mirroring the procfs sanitizers' fail-open
//! behavior on unknown schemas). It only ever *zeroes* fixed-width fields in
//! place; it never grows, shrinks, or shifts the buffer, so a partial parse can
//! never corrupt message boundaries.

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1064)

/// `sizeof(struct nlmsghdr)`.
const NLMSG_HDRLEN: usize = 16;
/// Netlink message alignment (`NLMSG_ALIGNTO`).
const NLMSG_ALIGNTO: usize = 4;
/// Routing-attribute header length (`sizeof(struct rtattr)`).
const RTA_HDRLEN: usize = 4;
/// Routing-attribute alignment (`RTA_ALIGNTO`).
const RTA_ALIGNTO: usize = 4;
/// `nlmsg_type` for a socket-diag payload message.
const SOCK_DIAG_BY_FAMILY: u16 = 20;

/// Offset of `udiag_ino` within `struct unix_diag_msg`.
const UNIX_DIAG_INO_OFFSET: usize = 4;
/// Offset of `udiag_cookie` within `struct unix_diag_msg`.
const UNIX_DIAG_COOKIE_OFFSET: usize = 8;
/// `sizeof(struct unix_diag_msg)` — attributes begin immediately after.
const UNIX_DIAG_MSG_LEN: usize = 16;
/// `UNIX_DIAG_PEER` attribute type (payload is the peer's `__u32` inode).
const UNIX_DIAG_PEER: u16 = 2;

/// Offset of `ndiag_ino` within `struct netlink_diag_msg`.
const NETLINK_DIAG_INO_OFFSET: usize = 16;
/// Offset of `ndiag_cookie` within `struct netlink_diag_msg`.
const NETLINK_DIAG_COOKIE_OFFSET: usize = 20;
/// `sizeof(struct netlink_diag_msg)`.
const NETLINK_DIAG_MSG_LEN: usize = 28;

/// Offset of `idiag_cookie` within `struct inet_diag_msg`, shared by `AF_INET`
/// and `AF_INET6`: four leading bytes followed by the first 40 bytes of
/// `struct inet_diag_sockid`.
const INET_DIAG_COOKIE_OFFSET: usize = 44;

/// Offset of `idiag_inode` within `struct inet_diag_msg`, shared by `AF_INET`
/// and `AF_INET6`: four leading bytes, then a 48-byte `inet_diag_sockid`, then
/// `idiag_expires`, `idiag_rqueue`, `idiag_wqueue` and `idiag_uid`.
///
/// Confirmed against the installed headers rather than counted by hand --
/// `offsetof(struct inet_diag_msg, idiag_inode)` is 68 and `sizeof` is 72.
const INET_DIAG_INO_OFFSET: usize = 68;
/// `sizeof(struct inet_diag_msg)`.
const INET_DIAG_MSG_LEN: usize = 72;

/// Each socket cookie is two adjacent `u32` words.
const SOCK_DIAG_COOKIE_LEN: usize = 8;

/// Zero every host-assigned socket identity in a `NETLINK_SOCK_DIAG` reply.
///
/// This includes inode numbers, two-word socket cookies, and `UNIX_DIAG_PEER`
/// inode attributes.
///
/// Returns `true` when `buf` was rewritten (at least one non-zero identity was
/// zeroed) and the stream parsed consistently. Returns `false` — leaving `buf`
/// unchanged — when the bytes do not match the expected netlink framing
/// (fail-open) or when there were no non-zero identities to zero.
pub fn sanitize_sock_diag_identities(buf: &mut [u8]) -> bool {
    match rewritten(buf) {
        Some(next) => {
            buf.copy_from_slice(&next);
            true
        }
        None => false,
    }
}

/// Round `value` up to the next multiple of `align` (a power of two).
fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

/// One exact, contiguous netlink message span (including its alignment
/// padding), so reordering whole spans preserves the buffer's total length.
struct Span {
    nlmsg_type: u16,
    nlmsg_len: usize,
    bytes: Vec<u8>,
}

/// Parse the multipart stream, zeroing identity fields and canonicalizing the
/// order of the diag messages. Returns `Some(copy)` when it parsed consistently
/// and changed something, otherwise `None` (fail-open / nothing to do).
fn rewritten(buf: &[u8]) -> Option<Vec<u8>> {
    // Split the stream into exact contiguous spans that tile `buf`; reordering
    // whole spans therefore preserves the total length exactly.
    let mut spans: Vec<Span> = Vec::new();
    let mut offset = 0usize;
    while offset < buf.len() {
        // A well-formed stream ends on a message boundary; a trailing remnant
        // too small for a header is a framing error -> fail open.
        if offset + NLMSG_HDRLEN > buf.len() {
            return None;
        }
        let nlmsg_len = u32::from_ne_bytes(buf[offset..offset + 4].try_into().ok()?) as usize;
        let nlmsg_type = u16::from_ne_bytes(buf[offset + 4..offset + 6].try_into().ok()?);
        if nlmsg_len < NLMSG_HDRLEN || offset + nlmsg_len > buf.len() {
            return None;
        }
        let advance = align_up(nlmsg_len, NLMSG_ALIGNTO);
        if advance == 0 {
            return None;
        }
        // The final message may lack trailing alignment padding.
        let span_end = (offset + advance).min(buf.len());
        spans.push(Span {
            nlmsg_type,
            nlmsg_len,
            bytes: buf[offset..span_end].to_vec(),
        });
        offset += advance;
    }

    let mut modified = false;

    // Zero the host-assigned socket identities inside every diag message.
    for span in &mut spans {
        if span.nlmsg_type == SOCK_DIAG_BY_FAMILY {
            let end = span.nlmsg_len.min(span.bytes.len());
            if NLMSG_HDRLEN < end {
                zero_family_identities(&mut span.bytes, NLMSG_HDRLEN, end, &mut modified);
            }
        }
    }

    // Canonicalize the order of the diag messages, mirroring the row-sort the
    // procfs text sanitizers apply. Only when the diag messages form a
    // contiguous prefix terminated by a non-diag trailer (the normal
    // `[diag*, NLMSG_DONE]` dump shape) — this guarantees every diag message
    // keeps its full alignment padding, so reordering cannot misalign the
    // stream. Any other shape is left in original order (fail-open).
    let diag_count = spans
        .iter()
        .filter(|s| s.nlmsg_type == SOCK_DIAG_BY_FAMILY)
        .count();
    let sortable = diag_count > 1
        && diag_count < spans.len()
        && spans[..diag_count]
            .iter()
            .all(|s| s.nlmsg_type == SOCK_DIAG_BY_FAMILY);
    if sortable {
        let mut order: Vec<usize> = (0..diag_count).collect();
        order.sort_by(|&a, &b| spans[a].bytes.cmp(&spans[b].bytes));
        if order.iter().enumerate().any(|(i, &j)| i != j) {
            let reordered: Vec<Span> = order
                .into_iter()
                .map(|j| Span {
                    nlmsg_type: spans[j].nlmsg_type,
                    nlmsg_len: spans[j].nlmsg_len,
                    bytes: spans[j].bytes.clone(),
                })
                .collect();
            for (slot, span) in spans[..diag_count].iter_mut().zip(reordered) {
                *slot = span;
            }
            modified = true;
        }
    }

    if !modified {
        return None;
    }
    let mut out = Vec::with_capacity(buf.len());
    for span in &spans {
        out.extend_from_slice(&span.bytes);
    }
    // Reordering whole spans must preserve the exact length; bail out otherwise.
    if out.len() != buf.len() {
        return None;
    }
    Some(out)
}

/// Zero the identity field(s) of one `SOCK_DIAG_BY_FAMILY` body, dispatched on
/// the address family stored in its first byte. The complete fixed-size family
/// body must be present before any field is changed, so an unknown or truncated
/// family is left untouched.
fn zero_family_identities(out: &mut [u8], body: usize, end: usize, modified: &mut bool) {
    let family = out[body] as i32;
    match family {
        libc::AF_UNIX => {
            if !fixed_body_is_present(body, UNIX_DIAG_MSG_LEN, end, out.len()) {
                return;
            }
            zero_u32(out, body + UNIX_DIAG_INO_OFFSET, end, modified);
            zero_bytes(
                out,
                body + UNIX_DIAG_COOKIE_OFFSET,
                SOCK_DIAG_COOKIE_LEN,
                end,
                modified,
            );
            zero_unix_peer_attrs(out, body + UNIX_DIAG_MSG_LEN, end, modified);
        }
        libc::AF_NETLINK => {
            if !fixed_body_is_present(body, NETLINK_DIAG_MSG_LEN, end, out.len()) {
                return;
            }
            zero_u32(out, body + NETLINK_DIAG_INO_OFFSET, end, modified);
            zero_bytes(
                out,
                body + NETLINK_DIAG_COOKIE_OFFSET,
                SOCK_DIAG_COOKIE_LEN,
                end,
                modified,
            );
        }
        // `ss -t`/`ss -u` and anything else reading the socket table. Both
        // families share `struct inet_diag_msg`, so they share the offset.
        libc::AF_INET | libc::AF_INET6 => {
            if !fixed_body_is_present(body, INET_DIAG_MSG_LEN, end, out.len()) {
                return;
            }
            zero_bytes(
                out,
                body + INET_DIAG_COOKIE_OFFSET,
                SOCK_DIAG_COOKIE_LEN,
                end,
                modified,
            );
            zero_u32(out, body + INET_DIAG_INO_OFFSET, end, modified);
        }
        // AF_VSOCK and AF_XDP also register socket-diag handlers on this
        // kernel, and their bodies also carry a host-assigned inode
        // (`vdiag_ino`, `xdiag_ino`). They are deliberately absent: no dump on
        // this host returns a single message for either, so the offsets could
        // not be checked against a real reply. This parser is zero-only and
        // bounds-checked, so a wrong offset would not fault -- it would
        // silently zero the wrong field and fail open, which is worse than an
        // acknowledged gap. Add them alongside a populated dump, not before.
        _ => {}
    }
}

fn fixed_body_is_present(body: usize, len: usize, end: usize, buffer_len: usize) -> bool {
    body.checked_add(len)
        .is_some_and(|body_end| body_end <= end && body_end <= buffer_len)
}

/// Walk the routing attributes trailing a `unix_diag_msg`, zeroing the peer
/// inode carried by any `UNIX_DIAG_PEER` attribute. Stops on any inconsistency;
/// because it only zeroes, an early stop is safe.
fn zero_unix_peer_attrs(out: &mut [u8], mut attr: usize, end: usize, modified: &mut bool) {
    while attr + RTA_HDRLEN <= end {
        let rta_len = u16::from_ne_bytes([out[attr], out[attr + 1]]) as usize;
        let rta_type = u16::from_ne_bytes([out[attr + 2], out[attr + 3]]);
        if rta_len < RTA_HDRLEN || attr + rta_len > end {
            break;
        }
        if rta_type == UNIX_DIAG_PEER && rta_len >= RTA_HDRLEN + 4 {
            zero_u32(out, attr + RTA_HDRLEN, end, modified);
        }
        let advance = align_up(rta_len, RTA_ALIGNTO);
        if advance == 0 {
            break;
        }
        attr += advance;
    }
}

/// Zero the `u32` at `at` when it is fully in bounds and currently non-zero.
fn zero_u32(out: &mut [u8], at: usize, end: usize, modified: &mut bool) {
    zero_bytes(out, at, 4, end, modified);
}

/// Zero an exact byte range when it is fully in bounds and currently non-zero.
fn zero_bytes(out: &mut [u8], at: usize, len: usize, end: usize, modified: &mut bool) {
    let Some(range_end) = at.checked_add(len) else {
        return;
    };
    if range_end <= end
        && range_end <= out.len()
        && out[at..range_end].iter().any(|&byte| byte != 0)
    {
        out[at..range_end].fill(0);
        *modified = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Append an `nlmsghdr` + body as one message, padded to `NLMSG_ALIGNTO`.
    fn push_message(buf: &mut Vec<u8>, nlmsg_type: u16, body: &[u8]) {
        let len = NLMSG_HDRLEN + body.len();
        buf.extend_from_slice(&(len as u32).to_ne_bytes()); // nlmsg_len
        buf.extend_from_slice(&nlmsg_type.to_ne_bytes()); // nlmsg_type
        buf.extend_from_slice(&2u16.to_ne_bytes()); // nlmsg_flags (NLM_F_MULTI)
        buf.extend_from_slice(&0u32.to_ne_bytes()); // nlmsg_seq
        buf.extend_from_slice(&0u32.to_ne_bytes()); // nlmsg_pid
        buf.extend_from_slice(body);
        while !buf.len().is_multiple_of(NLMSG_ALIGNTO) {
            buf.push(0);
        }
    }

    /// A `unix_diag_msg` (16 bytes) followed by a `UNIX_DIAG_PEER` attribute.
    fn unix_diag_body(ino: u32, cookie: [u32; 2], peer_ino: u32) -> Vec<u8> {
        let mut body = vec![
            libc::AF_UNIX as u8, // udiag_family
            0x5a,                // udiag_type sentinel
            1,                   // udiag_state
            0xa5,                // pad sentinel
        ];
        body.extend_from_slice(&ino.to_ne_bytes()); // udiag_ino
        for word in cookie {
            body.extend_from_slice(&word.to_ne_bytes()); // udiag_cookie[2]
        }
        assert_eq!(body.len(), UNIX_DIAG_MSG_LEN);
        // UNIX_DIAG_PEER attribute: rta_len, rta_type, u32 payload.
        let rta_len = (RTA_HDRLEN + 4) as u16;
        body.extend_from_slice(&rta_len.to_ne_bytes());
        body.extend_from_slice(&UNIX_DIAG_PEER.to_ne_bytes());
        body.extend_from_slice(&peer_ino.to_ne_bytes());
        body
    }

    /// A complete `netlink_diag_msg`.
    fn netlink_diag_body(ino: u32, cookie: [u32; 2]) -> Vec<u8> {
        let mut body = vec![
            libc::AF_NETLINK as u8, // ndiag_family
            0x5a,                   // ndiag_type sentinel
            0xa5,                   // ndiag_protocol sentinel
            1,                      // ndiag_state
        ];
        body.extend_from_slice(&0x4000_0000u32.to_ne_bytes()); // ndiag_portid
        body.extend_from_slice(&0x2233_4455u32.to_ne_bytes()); // ndiag_dst_portid
        body.extend_from_slice(&0x6677_8899u32.to_ne_bytes()); // ndiag_dst_group
        body.extend_from_slice(&ino.to_ne_bytes()); // ndiag_ino
        for word in cookie {
            body.extend_from_slice(&word.to_ne_bytes()); // ndiag_cookie[2]
        }
        assert_eq!(body.len(), NETLINK_DIAG_MSG_LEN);
        body
    }

    /// A complete `inet_diag_msg` with non-identity bytes left as sentinels.
    fn inet_diag_body(family: i32, ino: u32, cookie: [u32; 2]) -> Vec<u8> {
        let mut body = vec![0xa5; INET_DIAG_MSG_LEN];
        body[0] = family as u8;
        for (index, word) in cookie.into_iter().enumerate() {
            let at = INET_DIAG_COOKIE_OFFSET + index * 4;
            body[at..at + 4].copy_from_slice(&word.to_ne_bytes());
        }
        body[INET_DIAG_INO_OFFSET..INET_DIAG_INO_OFFSET + 4].copy_from_slice(&ino.to_ne_bytes());
        body
    }

    fn read_u32(buf: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap())
    }

    fn assert_only_identity_ranges_are_zeroed(
        mut body: Vec<u8>,
        identity_ranges: &[(usize, usize)],
    ) {
        let mut expected = body.clone();
        for &(start, len) in identity_ranges {
            expected[start..start + len].fill(0);
        }
        let mut modified = false;
        let end = body.len();
        zero_family_identities(&mut body, 0, end, &mut modified);
        assert!(modified);
        assert_eq!(body, expected, "non-identity sentinel bytes changed");
    }

    #[test]
    fn zeroes_unix_and_netlink_identities_and_peer() {
        let mut buf = Vec::new();
        push_message(
            &mut buf,
            SOCK_DIAG_BY_FAMILY,
            &unix_diag_body(0x1122_3344, [0x0102_0304, 0x0506_0708], 0x5566_7788),
        );
        let unix_body = NLMSG_HDRLEN;
        let netlink_start = buf.len();
        push_message(
            &mut buf,
            SOCK_DIAG_BY_FAMILY,
            &netlink_diag_body(0x99aa_bbcc, [0x1112_1314, 0x1516_1718]),
        );
        // A trailing NLMSG_DONE (type 3) as real dumps emit.
        push_message(&mut buf, 3, &0i32.to_ne_bytes());

        assert_eq!(
            read_u32(&buf, unix_body + UNIX_DIAG_INO_OFFSET),
            0x1122_3344
        );
        assert_eq!(
            read_u32(&buf, unix_body + UNIX_DIAG_COOKIE_OFFSET),
            0x0102_0304
        );

        assert!(sanitize_sock_diag_identities(&mut buf));

        assert_eq!(read_u32(&buf, unix_body + UNIX_DIAG_INO_OFFSET), 0);
        assert_eq!(
            &buf[unix_body + UNIX_DIAG_COOKIE_OFFSET
                ..unix_body + UNIX_DIAG_COOKIE_OFFSET + SOCK_DIAG_COOKIE_LEN],
            &[0; SOCK_DIAG_COOKIE_LEN]
        );
        assert_eq!(
            read_u32(&buf, unix_body + UNIX_DIAG_MSG_LEN + RTA_HDRLEN),
            0
        );
        assert_eq!(
            read_u32(&buf, netlink_start + NLMSG_HDRLEN + NETLINK_DIAG_INO_OFFSET),
            0
        );
        assert_eq!(
            &buf[netlink_start + NLMSG_HDRLEN + NETLINK_DIAG_COOKIE_OFFSET
                ..netlink_start + NLMSG_HDRLEN + NETLINK_DIAG_COOKIE_OFFSET + SOCK_DIAG_COOKIE_LEN],
            &[0; SOCK_DIAG_COOKIE_LEN]
        );
        assert_eq!(
            read_u32(&buf, netlink_start + NLMSG_HDRLEN + 4),
            0x4000_0000
        );
    }

    #[test]
    fn preserves_non_identity_sentinels_for_every_supported_family() {
        assert_only_identity_ranges_are_zeroed(
            unix_diag_body(0x1122_3344, [0x0102_0304, 0x0506_0708], 0x5566_7788),
            &[
                (UNIX_DIAG_INO_OFFSET, 4),
                (UNIX_DIAG_COOKIE_OFFSET, SOCK_DIAG_COOKIE_LEN),
                (UNIX_DIAG_MSG_LEN + RTA_HDRLEN, 4),
            ],
        );
        assert_only_identity_ranges_are_zeroed(
            netlink_diag_body(0x1122_3344, [0x0102_0304, 0x0506_0708]),
            &[
                (NETLINK_DIAG_INO_OFFSET, 4),
                (NETLINK_DIAG_COOKIE_OFFSET, SOCK_DIAG_COOKIE_LEN),
            ],
        );
        for family in [libc::AF_INET, libc::AF_INET6] {
            assert_only_identity_ranges_are_zeroed(
                inet_diag_body(family, 0x1122_3344, [0x0102_0304, 0x0506_0708]),
                &[
                    (INET_DIAG_COOKIE_OFFSET, SOCK_DIAG_COOKIE_LEN),
                    (INET_DIAG_INO_OFFSET, 4),
                ],
            );
        }
    }

    #[test]
    fn truncated_family_bodies_are_left_untouched() {
        let cases = [
            (
                unix_diag_body(1, [2, 3], 4),
                UNIX_DIAG_COOKIE_OFFSET + SOCK_DIAG_COOKIE_LEN - 1,
                UNIX_DIAG_MSG_LEN - 1,
            ),
            (
                netlink_diag_body(1, [2, 3]),
                NETLINK_DIAG_COOKIE_OFFSET + SOCK_DIAG_COOKIE_LEN - 1,
                NETLINK_DIAG_MSG_LEN - 1,
            ),
            (
                inet_diag_body(libc::AF_INET, 1, [2, 3]),
                INET_DIAG_COOKIE_OFFSET + SOCK_DIAG_COOKIE_LEN - 1,
                INET_DIAG_MSG_LEN - 1,
            ),
            (
                inet_diag_body(libc::AF_INET6, 1, [2, 3]),
                INET_DIAG_COOKIE_OFFSET + SOCK_DIAG_COOKIE_LEN - 1,
                INET_DIAG_MSG_LEN - 1,
            ),
        ];
        for (full, cookie_truncation, fixed_body_truncation) in cases {
            for truncated_len in [cookie_truncation, fixed_body_truncation] {
                let mut truncated = full[..truncated_len].to_vec();
                let original = truncated.clone();
                let mut modified = false;
                let end = truncated.len();
                zero_family_identities(&mut truncated, 0, end, &mut modified);
                assert!(!modified, "family={} len={truncated_len}", full[0]);
                assert_eq!(
                    truncated, original,
                    "family={} len={truncated_len}",
                    full[0]
                );
            }
        }
    }

    #[test]
    fn canonicalizes_diag_message_order() {
        let hi = netlink_diag_body(0x1111_1111, [0xaaaa_aaaa, 0xbbbb_bbbb]);
        let mut lo = netlink_diag_body(0x2222_2222, [0xcccc_cccc, 0xdddd_dddd]);
        // Give `lo` a smaller stable portid; inode and cookie fields must be
        // zero before these messages are compared and sorted.
        lo[4..8].copy_from_slice(&0x1000_0000u32.to_ne_bytes());

        let build = |first: &[u8], second: &[u8]| {
            let mut b = Vec::new();
            push_message(&mut b, SOCK_DIAG_BY_FAMILY, first);
            push_message(&mut b, SOCK_DIAG_BY_FAMILY, second);
            push_message(&mut b, 3, &0i32.to_ne_bytes()); // NLMSG_DONE
            b
        };

        let mut forward = build(&lo, &hi);
        let mut reverse = build(&hi, &lo);
        assert!(sanitize_sock_diag_identities(&mut forward));
        assert!(sanitize_sock_diag_identities(&mut reverse));
        assert_eq!(
            forward, reverse,
            "diag messages must be canonicalized to the same order regardless of arrival order"
        );
        assert_eq!(forward.len(), reverse.len());
    }

    #[test]
    fn already_zero_reports_no_change() {
        let mut buf = Vec::new();
        push_message(&mut buf, SOCK_DIAG_BY_FAMILY, &unix_diag_body(0, [0, 0], 0));
        assert!(!sanitize_sock_diag_identities(&mut buf));
    }

    #[test]
    fn fail_open_on_truncated_header() {
        let mut buf = vec![0xAB; NLMSG_HDRLEN - 1];
        let original = buf.clone();
        assert!(!sanitize_sock_diag_identities(&mut buf));
        assert_eq!(buf, original);
    }

    #[test]
    fn fail_open_on_inconsistent_length() {
        // nlmsg_len claims more bytes than the buffer holds.
        let mut buf = Vec::new();
        buf.extend_from_slice(&999u32.to_ne_bytes()); // nlmsg_len way past end
        buf.extend_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        buf.extend_from_slice(&0u16.to_ne_bytes());
        buf.extend_from_slice(&0u32.to_ne_bytes());
        buf.extend_from_slice(&0u32.to_ne_bytes());
        buf.extend_from_slice(&unix_diag_body(0x1234, [0x5678, 0x9abc], 0));
        let original = buf.clone();
        assert!(!sanitize_sock_diag_identities(&mut buf));
        assert_eq!(
            buf, original,
            "malformed framing must leave the buffer untouched"
        );
    }

    #[test]
    fn empty_buffer_is_noop() {
        let mut buf: Vec<u8> = Vec::new();
        assert!(!sanitize_sock_diag_identities(&mut buf));
    }

    #[test]
    fn non_diag_messages_are_untouched() {
        // An NLMSG_ERROR carrying identity-looking bytes must be preserved.
        let mut buf = Vec::new();
        let payload = unix_diag_body(0x1122_3344, [0x0102_0304, 0x0506_0708], 0x5566_7788);
        push_message(&mut buf, 2, &payload);
        let original = buf.clone();
        assert!(!sanitize_sock_diag_identities(&mut buf));
        assert_eq!(buf, original);
    }
}
