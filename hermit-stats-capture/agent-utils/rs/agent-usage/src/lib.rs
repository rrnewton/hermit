//! Cheap plan-usage, reset-time and burn-rate readings for the Claude Code and Codex CLIs.
//!
//! The readers use the structured sources the CLIs' own `/status` screens use, so a reading
//! costs no model call: Claude's claude.ai usage endpoint ([`claude`]) and Codex's app-server
//! `account/rateLimits/read` request ([`codex`]). Samples go to an append-only history
//! ([`history`]); [`burn`] derives burn rates and projections from it; [`transcripts`] keeps an
//! incremental index of local Claude Code token use; [`report`] renders it all; [`daemon`] polls
//! on an interval.

pub mod burn;
pub mod claude;
pub mod cli;
pub mod codex;
pub mod daemon;
pub mod history;
pub mod http;
pub mod limits;
pub mod model;
pub mod paths;
pub mod poll;
pub mod report;
pub mod timefmt;
pub mod transcripts;

/// Full reference, printed by `agent-usage userguide`.
pub const USER_GUIDE: &str = include_str!("embedded_userguide.md");

/// One-screen introduction, printed by `agent-usage quickstart`.
pub const QUICKSTART: &str = include_str!("embedded_quickstart.md");
