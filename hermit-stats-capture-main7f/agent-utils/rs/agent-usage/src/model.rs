//! The data model shared by the readers, the history file and the reports.

use serde::{Deserialize, Serialize};

/// Version written into every history line, so a later reader can tell old rows apart.
pub const SAMPLE_VERSION: u32 = 1;

/// The harnesses this tool reads.
pub const PROVIDERS: [&str; 2] = ["claude", "codex"];

/// Whether a reading produced plan meters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The provider answered and every meter it reported parsed.
    Ok,
    /// Plan limits do not apply here (no subscription login: an API key, a cloud provider or a
    /// gateway), so there is nothing to read. Not an error.
    Unavailable,
    /// A reading was attempted and failed (network, expired login, malformed reply).
    Error,
}

/// One plan-usage window, as the provider reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meter {
    /// Stable identifier used to match the same window across samples, such as `session`,
    /// `weekly_all`, `weekly:Fable` or `codex:primary`.
    pub id: String,
    /// The provider's own label, such as `Current session` or `Current week (Fable)`.
    pub label: String,
    /// Share of the window used, 0-100.
    pub used_pct: f64,
    /// Unix seconds when the window resets, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    /// Window length in minutes, when reported or implied by the meter kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_mins: Option<i64>,
}

/// Token counts. In a sample these are cumulative counters; in a report they are window totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    /// Model requests (assistant messages), when the source can count them.
    #[serde(default)]
    pub requests: u64,
    /// Uncached input tokens.
    #[serde(default)]
    pub input: u64,
    /// Output tokens, reasoning included.
    #[serde(default)]
    pub output: u64,
    /// Input tokens served from the prompt cache.
    #[serde(default)]
    pub cache_read: u64,
    /// Input tokens written to the prompt cache.
    #[serde(default)]
    pub cache_write: u64,
    /// Every token the source counts. For sources that only expose a grand total (codex's thread
    /// table) this is the only non-zero field.
    #[serde(default)]
    pub total: u64,
}

impl Tokens {
    /// Field-wise sum.
    pub fn add(&mut self, other: &Tokens) {
        self.requests += other.requests;
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.total += other.total;
    }

    /// Field-wise `self - earlier`, clamped at zero (a counter that went down contributes nothing).
    pub fn since(&self, earlier: &Tokens) -> Tokens {
        Tokens {
            requests: self.requests.saturating_sub(earlier.requests),
            input: self.input.saturating_sub(earlier.input),
            output: self.output.saturating_sub(earlier.output),
            cache_read: self.cache_read.saturating_sub(earlier.cache_read),
            cache_write: self.cache_write.saturating_sub(earlier.cache_write),
            total: self.total.saturating_sub(earlier.total),
        }
    }
}

/// One reading of one provider: a line of `history.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// History format version ([`SAMPLE_VERSION`]).
    pub v: u32,
    /// Unix seconds when the reading was taken.
    pub ts: i64,
    /// `claude` or `codex`.
    pub provider: String,
    /// Outcome of the plan-usage read.
    pub status: Status,
    /// Where the meters came from (`oauth-usage`, `app-server`), or why there are none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Human-readable reason for `unavailable` or `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Plan or subscription name, when the provider reports one (`max`, `plus`, `pro`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Plan-usage windows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub meters: Vec<Meter>,
    /// Cumulative local token counter, when the provider keeps one (codex's thread table). Burn
    /// is the difference between two samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_cumulative: Option<Tokens>,
    /// The provider asked not to be polled again before this time (Unix seconds), from an HTTP
    /// 429's `Retry-After`. Polls before then reuse the history instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_until: Option<i64>,
    /// Milliseconds the reading took.
    #[serde(default)]
    pub elapsed_ms: u64,
}

impl Sample {
    /// A sample with no meters yet.
    pub fn new(provider: &str, ts: i64, status: Status) -> Self {
        Sample {
            v: SAMPLE_VERSION,
            ts,
            provider: provider.to_string(),
            status,
            source: None,
            detail: None,
            plan: None,
            meters: Vec::new(),
            tokens_cumulative: None,
            backoff_until: None,
            elapsed_ms: 0,
        }
    }

    /// The meter with this id, if the sample has it.
    pub fn meter(&self, id: &str) -> Option<&Meter> {
        self.meters.iter().find(|m| m.id == id)
    }
}

/// A usage percentage is accepted only when it is a finite number in 0..=100 (a little slack
/// above 100 is allowed: providers report overage as values slightly past the cap).
pub fn valid_pct(value: f64) -> bool {
    value.is_finite() && (0.0..=200.0).contains(&value)
}
