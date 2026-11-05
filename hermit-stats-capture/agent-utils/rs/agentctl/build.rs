//! Records build provenance for `agentctl capabilities`.
//!
//! `AGENTCTL_SOURCE_FP` names the source fingerprint the binary is built from: the 64-character
//! lowercase hexadecimal SHA-256 that `common/bin/rs-source-fingerprint` prints. When it is set,
//! the value is checked and passed to the crate as `AGENTCTL_BUILD_SOURCE_FP`; when it is unset
//! or empty, the binary reports no fingerprint. A malformed value fails the build rather than
//! publishing provenance nobody can match. The commit is deliberately not recorded, so an
//! unrelated commit does not force a rebuild.

use std::env::{self, VarError};

const SOURCE_FINGERPRINT: &str = "AGENTCTL_SOURCE_FP";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={SOURCE_FINGERPRINT}");
    match env::var(SOURCE_FINGERPRINT) {
        Ok(value) if value.is_empty() => {}
        Ok(value) => {
            let hexadecimal = value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
            assert!(
                value.len() == 64 && hexadecimal,
                "{SOURCE_FINGERPRINT} must be a 64-character lowercase hexadecimal SHA-256, got {value:?}"
            );
            println!("cargo:rustc-env=AGENTCTL_BUILD_SOURCE_FP={value}");
        }
        Err(VarError::NotPresent) => {}
        Err(VarError::NotUnicode(value)) => {
            panic!("{SOURCE_FINGERPRINT} is not valid Unicode: {value:?}")
        }
    }
}
