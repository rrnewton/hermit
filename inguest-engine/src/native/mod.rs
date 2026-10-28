//! Common native mechanism entry into the one concrete Detcore engine.
//!
//! This module has no native backend dependency or policy alternative. Its
//! scalar callbacks are separate from any loader's sealed caller ABI. The
//! crate root compiles these entries in the shared engine; source activation
//! alone is not emitted-artifact, loader or native qualification.

mod abi;
mod auxv;
mod bootstrap;
mod config;
mod copied;
mod copied_policy;
mod diagnostics;
mod entry;
mod image;
mod lifecycle;
mod owner;
mod prelude;
mod policy;
