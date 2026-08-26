// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.

// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

pub const MANIFEST_SCHEMA: u64 = 3;
pub const DEFAULTS_FILE: &str = "defaults.yaml";
pub const MIN_TIMEOUT_SECONDS: u64 = 1;
pub const MAX_TIMEOUT_SECONDS: u64 = 1800;

#[allow(dead_code)]
pub fn validate_timeout_seconds(value: u64, context: &str) -> Result<u64, String> {
    if (MIN_TIMEOUT_SECONDS..=MAX_TIMEOUT_SECONDS).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "{context}: timeout_seconds must be {MIN_TIMEOUT_SECONDS}..={MAX_TIMEOUT_SECONDS}"
        ))
    }
}

pub fn resolve_timeout_seconds(
    global_default: u64,
    bucket_override: Option<u64>,
    cell_override: Option<u64>,
) -> u64 {
    cell_override.or(bucket_override).unwrap_or(global_default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_resolution_is_global_then_bucket_then_cell() {
        assert_eq!(resolve_timeout_seconds(15, None, None), 15);
        assert_eq!(resolve_timeout_seconds(15, Some(20), None), 20);
        assert_eq!(resolve_timeout_seconds(15, Some(20), Some(30)), 30);
    }

    #[test]
    fn timeout_bounds_are_closed() {
        assert_eq!(validate_timeout_seconds(1, "fixture").unwrap(), 1);
        assert_eq!(validate_timeout_seconds(1800, "fixture").unwrap(), 1800);
        assert!(validate_timeout_seconds(0, "fixture").is_err());
        assert!(validate_timeout_seconds(1801, "fixture").is_err());
    }
}
