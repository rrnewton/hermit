/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Canonicalize host identities in `NETLINK_SOCK_DIAG` dump replies.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const SOCK_DIAG_BY_FAMILY: u16 = 20;
const UNIX_DIAG_NAME: u16 = 0;
const UNIX_DIAG_VFS: u16 = 1;
const UNIX_DIAG_PEER: u16 = 2;
const UNIX_DIAG_ICONS: u16 = 3;
const UNIX_DIAG_UID: u16 = 7;
const NLA_TYPE_MASK: u16 = 0x3fff;
const NLMSG_HEADER_LEN: usize = 16;
const UNIX_DIAG_MESSAGE_LEN: usize = 16;
const NETLINK_DIAG_MESSAGE_LEN: usize = 28;
const NETLINK_DIAG_INO_OFFSET: usize = 16;

pub(crate) const NETLINK_SOCK_DIAG_PROTOCOL: i32 = 4;

fn align_netlink(length: usize) -> Option<usize> {
    length.checked_add(3).map(|value| value & !3)
}

fn read_ne_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_ne_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn normalize_private_socket_name(name: &mut [u8], private_socket: Option<&Path>) -> bool {
    const REPLACEMENT: &[u8] = b"/hermit-private-control.sock";

    let Some(private_socket) = private_socket else {
        return false;
    };
    let path = private_socket.as_os_str().as_bytes();
    if path.is_empty() {
        return false;
    }
    let Some(start) = name.windows(path.len()).position(|window| window == path) else {
        return false;
    };
    let normalized = &mut name[start..start + path.len()];
    normalized.fill(b'_');
    let visible = REPLACEMENT.len().min(normalized.len());
    normalized[..visible].copy_from_slice(&REPLACEMENT[..visible]);
    true
}

fn canonicalize_unix_diag_attributes(
    attributes: &mut [u8],
    private_socket: Option<&Path>,
) -> Result<usize, ()> {
    let mut offset = 0;
    let mut rewritten = 0;
    while offset < attributes.len() {
        if attributes.len() - offset < 4 {
            return attributes[offset..]
                .iter()
                .all(|byte| *byte == 0)
                .then_some(rewritten)
                .ok_or(());
        }
        let length = usize::from(read_ne_u16(attributes, offset).ok_or(())?);
        let kind = read_ne_u16(attributes, offset + 2).ok_or(())? & NLA_TYPE_MASK;
        if length < 4 {
            return Err(());
        }
        let end = offset.checked_add(length).ok_or(())?;
        if end > attributes.len() {
            return Err(());
        }
        let payload = &mut attributes[offset + 4..end];
        let changed = match kind {
            UNIX_DIAG_NAME => normalize_private_socket_name(payload, private_socket),
            UNIX_DIAG_VFS if payload.len() >= 8 => {
                payload[..8].fill(0);
                true
            }
            UNIX_DIAG_PEER | UNIX_DIAG_UID if payload.len() >= 4 => {
                payload[..4].fill(0);
                true
            }
            UNIX_DIAG_ICONS if payload.len().is_multiple_of(4) => {
                payload.fill(0);
                true
            }
            _ => false,
        };
        rewritten += usize::from(changed);

        let aligned = align_netlink(length).ok_or(())?;
        let next = offset.checked_add(aligned).ok_or(())?;
        if next > attributes.len() {
            if end == attributes.len() {
                return Ok(rewritten);
            }
            return Err(());
        }
        offset = next;
    }
    Ok(rewritten)
}

fn canonicalize_messages_inner(
    bytes: &mut [u8],
    private_socket: Option<&Path>,
) -> Result<usize, ()> {
    let mut offset = 0;
    let mut rewritten = 0;
    let mut current_run = Vec::new();
    let mut diag_runs = Vec::new();
    while offset < bytes.len() {
        if bytes.len() - offset < NLMSG_HEADER_LEN {
            if bytes[offset..].iter().all(|byte| *byte == 0) {
                break;
            }
            return Err(());
        }
        let length = usize::try_from(read_ne_u32(bytes, offset).ok_or(())?).map_err(|_| ())?;
        if length < NLMSG_HEADER_LEN {
            return Err(());
        }
        let end = offset.checked_add(length).ok_or(())?;
        if end > bytes.len() {
            return Err(());
        }
        let kind = read_ne_u16(bytes, offset + 4).ok_or(())?;
        let body = offset + NLMSG_HEADER_LEN;
        let family = (kind == SOCK_DIAG_BY_FAMILY)
            .then(|| bytes.get(body).copied())
            .flatten()
            .map(i32::from);
        let is_canonicalized_diag = match family {
            Some(libc::AF_UNIX) => {
                let attributes = body + UNIX_DIAG_MESSAGE_LEN;
                if attributes > end {
                    return Err(());
                }
                // unix_diag_msg::udiag_ino and both udiag_cookie words.
                bytes[body + 4..attributes].fill(0);
                rewritten += 1;
                rewritten +=
                    canonicalize_unix_diag_attributes(&mut bytes[attributes..end], private_socket)?;
                true
            }
            Some(libc::AF_NETLINK) => {
                if body + NETLINK_DIAG_MESSAGE_LEN > end {
                    return Err(());
                }
                // netlink_diag_msg::ndiag_ino and both ndiag_cookie words.
                bytes[body + NETLINK_DIAG_INO_OFFSET..body + NETLINK_DIAG_MESSAGE_LEN].fill(0);
                rewritten += 1;
                true
            }
            _ => false,
        };

        let aligned = align_netlink(length).ok_or(())?;
        let next = offset.checked_add(aligned).ok_or(())?;
        let span_end = if next > bytes.len() {
            if end == bytes.len() {
                end
            } else {
                return Err(());
            }
        } else {
            next
        };
        if is_canonicalized_diag {
            current_run.push((offset, span_end));
        } else if !current_run.is_empty() {
            diag_runs.push(std::mem::take(&mut current_run));
        }
        offset = span_end;
    }
    if !current_run.is_empty() {
        diag_runs.push(current_run);
    }

    for run in diag_runs.into_iter().filter(|run| run.len() > 1) {
        let start = run[0].0;
        let end = run[run.len() - 1].1;
        let mut messages = run
            .into_iter()
            .map(|(start, end)| bytes[start..end].to_vec())
            .collect::<Vec<_>>();
        messages.sort_unstable();
        let mut cursor = start;
        for message in messages {
            let next = cursor + message.len();
            bytes[cursor..next].copy_from_slice(&message);
            cursor = next;
        }
        debug_assert_eq!(cursor, end);
    }
    Ok(rewritten)
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1023): Review SOCK_DIAG identity canonicalization.
/// Canonicalize supported socket-diag messages, restoring the input on malformed framing.
pub(crate) fn canonicalize_messages(bytes: &mut [u8], private_socket: Option<&Path>) -> usize {
    let original = bytes.to_vec();
    match canonicalize_messages_inner(bytes, private_socket) {
        Ok(rewritten) => rewritten,
        Err(()) => {
            bytes.copy_from_slice(&original);
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_netlink_attribute(message: &mut Vec<u8>, kind: u16, payload: &[u8]) {
        let length = 4 + payload.len();
        message.extend_from_slice(&(length as u16).to_ne_bytes());
        message.extend_from_slice(&kind.to_ne_bytes());
        message.extend_from_slice(payload);
        message.resize(align_netlink(message.len()).unwrap(), 0);
    }

    fn unix_diag_message(name: &[u8]) -> Vec<u8> {
        let mut message = vec![0; NLMSG_HEADER_LEN];
        message[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        message.push(libc::AF_UNIX as u8);
        message.extend_from_slice(&[libc::SOCK_STREAM as u8, 1, 0]);
        message.extend_from_slice(&0x1122_3344_u32.to_ne_bytes());
        message.extend_from_slice(&0x5566_7788_u32.to_ne_bytes());
        message.extend_from_slice(&0x99aa_bbcc_u32.to_ne_bytes());
        append_netlink_attribute(&mut message, UNIX_DIAG_NAME, name);
        append_netlink_attribute(&mut message, UNIX_DIAG_PEER, &0xddee_ff00_u32.to_ne_bytes());
        let length = message.len() as u32;
        message[..4].copy_from_slice(&length.to_ne_bytes());
        message
    }

    fn netlink_diag_message() -> Vec<u8> {
        let mut message = vec![0; NLMSG_HEADER_LEN];
        message[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        message.push(libc::AF_NETLINK as u8);
        message.extend_from_slice(&[0, 0, 1]);
        message.extend_from_slice(&0x4000_0000_u32.to_ne_bytes());
        message.extend_from_slice(&0_u32.to_ne_bytes());
        message.extend_from_slice(&0_u32.to_ne_bytes());
        message.extend_from_slice(&0x1122_3344_u32.to_ne_bytes());
        message.extend_from_slice(&0x5566_7788_u32.to_ne_bytes());
        message.extend_from_slice(&0x99aa_bbcc_u32.to_ne_bytes());
        let length = message.len() as u32;
        message[..4].copy_from_slice(&length.to_ne_bytes());
        message
    }

    #[test]
    fn unix_diag_hides_kernel_and_private_control_socket_identities() {
        let private_socket = Path::new("/tmp/private-control-Ab12Z9/coordinator.sock");
        let mut message = unix_diag_message(b"/tmp/private-control-Ab12Z9/coordinator.sock\0");

        assert_eq!(canonicalize_messages(&mut message, Some(private_socket)), 3);
        assert_eq!(&message[20..32], &[0; 12]);
        assert!(
            message
                .windows(b"/hermit-private-control.sock".len())
                .any(|window| window == b"/hermit-private-control.sock")
        );
        let peer = message
            .windows(8)
            .find(|window| read_ne_u16(window, 2) == Some(UNIX_DIAG_PEER))
            .expect("peer attribute");
        assert_eq!(&peer[4..8], &[0; 4]);
    }

    #[test]
    fn netlink_diag_hides_inode_and_cookie_but_preserves_port_id() {
        let mut message = netlink_diag_message();

        assert_eq!(canonicalize_messages(&mut message, None), 1);
        assert_eq!(read_ne_u32(&message, 20), Some(0x4000_0000));
        assert_eq!(&message[32..44], &[0; 12]);
    }

    #[test]
    fn unix_diag_preserves_unrelated_names_and_non_diag_payloads() {
        let mut unix = unix_diag_message(b"/run/application.sock\0");
        let unix_before_name = b"/run/application.sock\0".to_vec();
        assert_eq!(canonicalize_messages(&mut unix, None), 2);
        assert!(
            unix.windows(unix_before_name.len())
                .any(|window| window == unix_before_name)
        );

        let mut unrelated = vec![16, 0, 0, 0, 99, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        let original = unrelated.clone();
        assert_eq!(canonicalize_messages(&mut unrelated, None), 0);
        assert_eq!(unrelated, original);
    }

    #[test]
    fn diag_records_are_sorted_after_identity_normalization() {
        let first = unix_diag_message(b"/run/z.sock\0");
        let mut second = unix_diag_message(b"/run/a.sock\0");
        second[18] = 2;
        let mut expected_records = vec![first.clone(), second.clone()];
        for record in &mut expected_records {
            assert_eq!(canonicalize_messages(record, None), 2);
        }
        expected_records.sort_unstable();
        let expected = expected_records.concat();

        let mut combined = [second, first].concat();
        assert_eq!(canonicalize_messages(&mut combined, None), 4);
        assert_eq!(combined, expected);
    }

    #[test]
    fn malformed_diag_fails_open() {
        let private_socket = Path::new("/tmp/private-control-abcdef/coordinator.sock");
        let mut truncated = unix_diag_message(b"/tmp/private-control-abcdef/coordinator.sock\0");
        truncated.truncate(truncated.len() - 1);
        let original = truncated.clone();

        assert_eq!(
            canonicalize_messages(&mut truncated, Some(private_socket)),
            0
        );
        assert_eq!(truncated, original);
    }
}
