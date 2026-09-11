//! Build-only root for shared Detcore LiteInst artifacts and verified provenance.
//!
//! The shared Detcore runtime is outside Hermit's Cargo workspace. This
//! standalone locked graph builds and validates it as an explicit artifact
//! without linking its constructor into the Hermit host.

//! The build script selects the exact Cargo-reported runtime output. Private mode
//! compiles owned native sources and links the static PIE using verified external
//! GNU inputs before generating provenance and staging the checked pair.

#[cfg(test)]
#[path = "../artifact.rs"]
mod artifact;

#[cfg(test)]
#[path = "../../hermit-cli/src/liteinst_artifact.rs"]
pub mod liteinst_artifact;
