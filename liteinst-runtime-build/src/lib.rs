//! Build-only root that stages the owned LiteInst preload.
//!
//! The published `reverie-liteinst-preload` leaf owns its allocator and
//! constructor. This standalone locked graph builds that leaf explicitly
//! without linking its constructor into the Hermit host.

//! The build script uses an isolated target directory and stages the exact
//! current leaf cdylib reported by that Cargo invocation.

// The producer's source authority requires one direct qualifier dependency,
// of build kind. The test-only support crate reexports that same pinned API.
#[cfg(test)]
extern crate hermit_native_finalizer_test_support as reverie_liteinst_artifact;

#[cfg(test)]
#[path = "../artifact.rs"]
mod artifact;

#[test]
fn finalizer_tests_use_the_pinned_qualifier_source() {
    // Independently fixed from the selected 55d215b3 commit's lib.rs, not the test reexport.
    assert_eq!(
        reverie_liteinst_artifact::qualifier_source_sha256(),
        "22c116ba09e2268feb8f0b88090cd683775553d8d558e91e58aaa4bab6fb2172"
    );
}
