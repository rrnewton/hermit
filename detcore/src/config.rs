/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore configuration and widely used types.
pub use detcore_model::config::CONFIG_FINGERPRINT_ENV;
pub use detcore_model::config::LITEINST_CONFIG_FINGERPRINT_ENV;
pub use detcore_model::config::*;

/// The source that defines Detcore's global RPC: `GlobalRequest`,
/// `GlobalResponse`, and the types they carry that it defines.
const GLOBAL_RPC_DEFINITION_SOURCES: &[&[u8]] = &[include_bytes!("tool_global.rs")];

/// [`detcore_model::config::config_wire_fingerprint`], extended with the
/// definitions of Detcore's global RPC.
///
/// A runtime that embeds Detcore (SaBRe's plugin, the LiteInst runtime that
/// in-guest-trap also loads, DBT's) sends `GlobalRequest`s to its coordinator
/// as positional bincode and decodes the `GlobalResponse`s. Both enums live
/// in this crate, outside the sources the configuration's fingerprint hashes,
/// so a variant one build had and the other lacked went unnoticed: a newer
/// runtime's request failed to decode in the middle of a run, and an older
/// runtime never sent it (and so, for one, kept the host's inotify rename
/// cookies). Every such runtime compares this fingerprint with its
/// coordinator's before it decodes anything, so the pair is refused by name.
/// As for the configuration's sources, the whole file is hashed: an edit to it
/// can require rebuilding a runtime that did not need it, which a runtime
/// embedding Detcore usually needs anyway.
pub fn config_wire_fingerprint() -> String {
    fingerprint_with_sources(
        &detcore_model::config::config_wire_fingerprint(),
        GLOBAL_RPC_DEFINITION_SOURCES,
    )
}

/// What differs when a coordinator announced `expected` as its
/// [`config_wire_fingerprint`] and this build's is another, or `None` when
/// they are equal. The runtimes report this before they decode the
/// coordinator's configuration (<https://github.com/rrnewton/hermit/issues/3986>).
pub fn config_fingerprint_mismatch(expected: &std::ffi::OsStr) -> Option<String> {
    let ours = config_wire_fingerprint();
    (expected.to_str() != Some(ours.as_str())).then(|| {
        format!(
            "its configuration and clock RPC fingerprint is {ours}, Hermit's is {}",
            expected.to_string_lossy()
        )
    })
}

/// FNV-1a over `base` and then each of `sources`, each length-prefixed in a
/// domain of its own, as `detcore_model`'s fingerprint hashes its material.
/// A mismatch detector, not a security boundary.
fn fingerprint_with_sources(base: &str, sources: &[&[u8]]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut update = |domain: u8, bytes: &[u8]| {
        for byte in std::iter::once(&domain)
            .chain((bytes.len() as u64).to_le_bytes().iter())
            .chain(bytes.iter())
        {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
    };
    update(0, base.as_bytes());
    for source in sources {
        update(1, source);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A change to the global RPC's definitions, such as a request variant
    /// appended for a new syscall's state, changes the fingerprint the
    /// runtimes compare, so a runtime from one side of it and a coordinator
    /// from the other refuse each other.
    #[test]
    fn the_fingerprint_covers_the_global_rpc_definitions() {
        let model = detcore_model::config::config_wire_fingerprint();
        let ours = config_wire_fingerprint();
        assert_eq!(ours.len(), 16);
        assert_eq!(ours, config_wire_fingerprint(), "stable");
        assert_eq!(
            ours,
            fingerprint_with_sources(&model, GLOBAL_RPC_DEFINITION_SOURCES)
        );
        assert_ne!(ours, model, "the RPC definitions are hashed");

        let source = include_str!("tool_global.rs");
        let request = "pub enum GlobalRequest {";
        assert!(
            source.contains(request),
            "the hashed file defines the requests"
        );
        let added = source.replacen(request, "pub enum GlobalRequest {\n    Added(u32),", 1);
        assert_ne!(
            fingerprint_with_sources(&model, &[added.as_bytes()]),
            ours,
            "a request added to the enum changes the fingerprint"
        );
        assert_ne!(
            fingerprint_with_sources("0123456789abcdef", GLOBAL_RPC_DEFINITION_SOURCES),
            ours,
            "so does the configuration's fingerprint"
        );

        assert_eq!(
            config_fingerprint_mismatch(std::ffi::OsStr::new(&ours)),
            None
        );
        let other = config_fingerprint_mismatch(std::ffi::OsStr::new(&model))
            .expect("the configuration's fingerprint alone is a mismatch");
        assert!(other.contains(&ours) && other.contains(&model), "{other}");
    }
}
