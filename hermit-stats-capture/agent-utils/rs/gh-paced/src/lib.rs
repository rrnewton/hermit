//! Client-side rate limiting for the GitHub CLI.
//!
//! `gh-paced` runs between an account shim and the real `gh`. It classifies each invocation
//! (READ, SEARCH, WRITE, GIT_CREDENTIAL, LOCAL), charges it against per-host, per-account token
//! buckets and hourly windows kept in a locked state file, sleeps with a loud warning when a
//! budget is exhausted, refuses waits longer than a bound, watches the account-wide numbers from
//! `GET /rate_limit`, backs off after any GitHub rate-limit or abuse response, and refuses write
//! bodies that are too large or carry encoded payloads, including text typed in the editor gh
//! opens ([`editor`]). gh's output passes through unchanged, credentials gh prints included;
//! stderr is scanned for GitHub's refusals, and the audit log keeps a redacted argument summary
//! with no bodies or environment.
//!
//! The pacing engine ([`wrapper::Wrapper`]) takes its clock and process runner as trait objects
//! so the whole decision path can be driven deterministically in tests.

pub mod alias;
pub mod audit;
pub mod budget;
pub mod classify;
pub mod cli;
pub mod clock;
pub mod config;
pub mod editor;
pub mod guard;
pub mod pushback;
pub mod ratelimit;
pub mod runner;
pub mod snapshot;
pub mod state;
pub mod status;
pub mod timefmt;
pub mod wrapper;

/// Full reference, printed by `gh-paced userguide`.
pub const USER_GUIDE: &str = include_str!("embedded_userguide.md");

/// One-screen introduction, printed by `gh-paced quickstart`.
pub const QUICKSTART: &str = include_str!("embedded_quickstart.md");
