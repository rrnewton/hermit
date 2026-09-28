//! Build-only root for shared Detcore LiteInst artifacts and verified provenance.
//!
//! `reverie-liteinst` is outside Hermit's Cargo workspace, so Hermit's normal
//! dependency edge cannot select features when Cargo builds that package as a
//! cdylib. This standalone locked graph makes the constructor-bearing runtime
//! an explicit artifact without linking its constructor into the Hermit host.

//! The build script selects the exact Cargo-reported runtime output. Private mode
//! compiles owned native sources and links the static PIE using verified external
//! GNU inputs before generating provenance and staging the checked pair.

#[cfg(test)]
#[path = "../artifact.rs"]
mod artifact;

#[cfg(test)]
#[path = "../legacy_artifact.rs"]
mod legacy_artifact;

#[cfg(test)]
#[path = "../../hermit-cli/src/liteinst_artifact.rs"]
pub mod liteinst_artifact;

#[cfg(test)]
#[path = "../../hermit-install/liteinst_inputs.rs"]
pub mod install_inputs;
