//! A deterministic, embeddable scheduled-tick engine.
//!
//! `tick-hub` evaluates cadenced reminders, optional shell gates, and filesystem freshness checks,
//! then emits a stable line protocol (`HEALTH:`, `ACTION:`, `NOTE:`, and `ERROR:`). The engine is
//! side-effect free apart from explicitly injected [`GateRunner`] and [`FileAgeProbe`] boundaries.

pub mod cadence;
pub mod cli;
pub mod emit;
pub mod engine;
pub mod io;
pub mod model;
pub mod probes;
pub mod protocols;
pub mod state;
mod text;

pub use cadence::{due_reminders, is_due, load_fired_state, persist_fired_state};
pub use cli::{render_list, run as run_cli, DEFAULT_FIRED_STATE, STATE_FILE_ENV};
pub use emit::{
    format_action, format_clean, format_error, format_health, format_note, format_suppressed,
    HEALTH_STATUS_MISSING, HEALTH_STATUS_OK, HEALTH_STATUS_STALE,
};
pub use engine::{
    evaluate_health, parse_kv_lines, render_emit, run_tick, run_tick_with_emit, TickResult,
    UnresolvedPlaceholderError,
};
pub use io::{config_from_json, config_from_yaml, config_to_json, config_to_yaml, TickConfigError};
pub use model::{Emit, EmitKind, Gate, GateWhen, HealthCheck, Reminder, TickConfig, EVERY_TICK};
pub use probes::{
    wall_clock_now, GlobFileAgeProbe, SubprocessGateRunner, DEFAULT_GATE_TIMEOUT_SECS,
};
pub use protocols::{FileAgeProbe, GateResult, GateRunner};
pub use state::{
    flag_truthy, state_lines, FlagValue, OpsState, StateError, DEFAULT_TICK_FREQUENCY_MIN,
};

/// Installed command name.
pub const PROG: &str = "tick-hub";

/// Crate and CLI version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Complete user guide embedded in the crate artifact.
pub const USER_GUIDE: &str = include_str!("embedded_userguide.md");
