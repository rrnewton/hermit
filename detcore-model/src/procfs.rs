/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Types and parsers shared by procfs producers and consumers.

/// Parse the one decimal `mnt_id` field required by Linux fdinfo.
///
/// Missing, duplicate, malformed, signed, or out-of-range values are rejected.
/// Keeping this byte parser below both `hermit-cli` and `detcore` prevents the
/// container capture path from accepting input the guest-visible sanitizer
/// would later refuse.
pub fn parse_fdinfo_mount_id(contents: &[u8]) -> Option<u64> {
    let mut mount_id = None;
    for line in contents.split(|byte| *byte == b'\n') {
        let Some(value) = line.strip_prefix(b"mnt_id:") else {
            continue;
        };
        let value = value
            .strip_prefix(b"\t")
            .or_else(|| value.strip_prefix(b" "))?;
        if value.is_empty() || value.iter().any(|byte| !byte.is_ascii_digit()) || mount_id.is_some()
        {
            return None;
        }
        let text = std::str::from_utf8(value).ok()?;
        mount_id = Some(text.parse().ok()?);
    }
    mount_id
}

#[cfg(test)]
mod tests {
    use super::parse_fdinfo_mount_id;

    #[test]
    fn fdinfo_mount_id_is_strict_and_unique() {
        assert_eq!(parse_fdinfo_mount_id(b"pos:\t0\nmnt_id:\t37\n"), Some(37));
        assert_eq!(parse_fdinfo_mount_id(b"mnt_id: 0\n"), Some(0));
        for malformed in [
            b"pos:\t0\n".as_slice(),
            b"mnt_id:\tbad\nmnt_id:\t37\n".as_slice(),
            b"mnt_id:\t37\nmnt_id:\t38\n".as_slice(),
            b"mnt_id:\t37 trailing\n".as_slice(),
            b"mnt_id:\t18446744073709551616\n".as_slice(),
            b"mnt_id:37\n".as_slice(),
        ] {
            assert_eq!(parse_fdinfo_mount_id(malformed), None, "{malformed:?}");
        }
    }
}
