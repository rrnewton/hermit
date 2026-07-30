//! Build-only root that enables the LiteInst preload constructor.
//!
//! `reverie-liteinst` is outside Hermit's Cargo workspace, so Hermit's normal
//! dependency edge cannot select features when Cargo builds that package as a
//! cdylib. This standalone locked graph makes the constructor-bearing runtime
//! an explicit artifact without linking its constructor into the Hermit host.

// The constructor-enabled runtime is a build dependency so Cargo must finish
// it before this package's build script validates and stages the cdylib.
