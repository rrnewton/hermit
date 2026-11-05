//! Allowlisted, audited command execution through a terminal pane the caller does not own.
//!
//! The pane belongs to a separate terminal server and is not a child of the calling process, so
//! whatever constrains that process does not constrain the command: not its network policy, not
//! its environment, not its lifetime. An agent whose sandbox blocks a destination it legitimately
//! needs is ONE use of that, and the one `herdr-run net-doctor` checks; it is an example, not the
//! definition.
//!
//! The crate exposes the policy and transport layers used by the `herdr-run` command so callers
//! can validate configuration and admission without starting a Herdr session.

pub mod allowlist;
pub mod audit;
pub mod cli;
pub mod client;
pub mod config;
pub mod error;
pub mod evict;
pub mod identity;
pub mod init;
pub mod output;
pub mod readiness;
pub mod reap;
pub mod retention;
pub mod runner;
pub mod session;
pub mod status;
pub mod sweep;
pub mod textfmt;
pub mod yaml;

mod state;
mod timefmt;

/// Full installed-package reference, also printed by `herdr-run userguide`.
pub const USER_GUIDE: &str = include_str!("embedded_userguide.md");

/// One-screen introduction, printed by `herdr-run quickstart`.
pub const QUICKSTART: &str = include_str!("embedded_quickstart.md");
