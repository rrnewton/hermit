//! Budgets and thresholds: built-in defaults, an optional JSON config file, and tighten-only
//! environment overrides.
//!
//! The defaults assume up to four hosts share one GitHub account and each host runs its own
//! `gh-paced` state. Every per-host number is chosen so that four hosts together stay well under
//! GitHub's documented limits; see the user guide for the arithmetic.

use crate::classify::Class;
use crate::timefmt::DisplayTz;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Rate limits for one request class on one host for one account.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ClassLimits {
    /// Sustained refill rate of the token bucket, in tokens per minute.
    pub per_minute: f64,
    /// Bucket capacity: how many tokens may be spent back to back after a quiet period.
    pub burst: f64,
    /// Hard cap on tokens spent in any sliding 3,600-second window.
    pub per_hour: u32,
}

/// The effective configuration for one invocation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Config {
    /// READ class limits (`gh api` GET, `pr view`, `run list`, ...).
    pub read: ClassLimits,
    /// SEARCH class limits (`gh search ...`, `gh api search/...`).
    pub search: ClassLimits,
    /// WRITE class limits (anything that may create or change content; unknown commands).
    pub write: ClassLimits,
    /// GIT_CREDENTIAL class limits (`gh auth git-credential get`, one per git network operation).
    pub git_credential: ClassLimits,
    /// Maximum concurrent WRITE invocations per host per account.
    pub write_max_in_flight: u32,
    /// Tokens charged for `gh api --paginate` / `--slurp` (one invocation may fetch many pages).
    pub paginate_cost: u32,
    /// Tokens charged for a watch loop (`gh pr checks --watch`, `gh run watch`).
    pub watch_cost: u32,
    /// Shortest watch polling interval allowed without `GH_PACED_ALLOW_FAST_WATCH=1`, in seconds.
    pub min_watch_interval_secs: f64,
    /// Longest total time one invocation may sleep before it is refused (exit 75), in seconds.
    pub max_wait_secs: f64,
    /// Longest total time one GIT_CREDENTIAL call may wait before it is refused (exit 75), in
    /// seconds. The smaller of this and `max_wait_secs` applies to that class, so it only ever
    /// shortens the wait. git runs the credential helper inside an operation that may hold its
    /// caller's locks, so the helper must fail fast instead of sleeping (a GIT_CREDENTIAL call
    /// is also refused at once, without sleeping, during a cooldown).
    pub git_credential_max_wait_secs: f64,
    /// Longest wait for another gh-paced process to release the account's state lock before
    /// failing with exit 70, in seconds (see `state::LOCK_WAIT_SECS`).
    pub lock_wait_secs: f64,
    /// Cooldown after GitHub reports a rate limit, abuse detection, or HTTP 429, in seconds.
    pub cooldown_secs: f64,
    /// Cooldown after a plain HTTP 403 with no rate-limit wording, in seconds.
    pub plain_403_cooldown_secs: f64,
    /// Refresh the `GET /rate_limit` snapshot when it is at least this old, in seconds.
    pub rate_limit_refresh_secs: f64,
    /// Refresh the snapshot after this many admitted calls since the last refresh.
    pub rate_limit_refresh_calls: u32,
    /// Never refresh the snapshot more often than this, in seconds.
    pub rate_limit_min_refresh_secs: f64,
    /// Give up on one `gh api rate_limit` refresh after this many seconds.
    pub rate_limit_timeout_secs: f64,
    /// Halve every rate when GitHub reports this fraction (or less) of the limit remaining.
    pub halve_below_fraction: f64,
    /// Block every API call until reset when this fraction (or less) remains.
    pub block_below_fraction: f64,
    /// Largest WRITE body (sum of all body sources) allowed, in bytes.
    pub max_body_bytes: usize,
    /// Longest base64-looking run allowed in a WRITE body, in characters.
    pub max_base64_run: usize,
    /// Clock used in messages.
    pub display_tz: DisplayTz,
    /// `GH_PACED_ALLOW_LARGE_BODY=1`: skip the content guard for this invocation.
    pub allow_large_body: bool,
    /// `GH_PACED_ALLOW_FAST_WATCH=1`: allow watch intervals shorter than the minimum.
    pub allow_fast_watch: bool,
    /// `GH_PACED_ALLOW_WATCH=1`: run watch loops (`gh pr checks --watch`, `gh run watch`) on an
    /// estimated charge. Without it they are refused, because gh-paced cannot count the requests
    /// a watch makes.
    pub allow_watch: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            read: ClassLimits {
                per_minute: 20.0,
                burst: 10.0,
                per_hour: 500,
            },
            search: ClassLimits {
                per_minute: 5.0,
                burst: 2.0,
                per_hour: 150,
            },
            write: ClassLimits {
                per_minute: 2.0,
                burst: 1.0,
                per_hour: 30,
            },
            git_credential: ClassLimits {
                per_minute: 6.0,
                burst: 1.0,
                per_hour: 120,
            },
            write_max_in_flight: 1,
            paginate_cost: 10,
            watch_cost: 20,
            min_watch_interval_secs: 30.0,
            max_wait_secs: 900.0,
            git_credential_max_wait_secs: 30.0,
            lock_wait_secs: crate::state::LOCK_WAIT_SECS,
            cooldown_secs: 900.0,
            plain_403_cooldown_secs: 900.0,
            rate_limit_refresh_secs: 300.0,
            rate_limit_refresh_calls: 50,
            rate_limit_min_refresh_secs: 60.0,
            rate_limit_timeout_secs: 20.0,
            halve_below_fraction: 0.5,
            block_below_fraction: 0.2,
            max_body_bytes: 8192,
            max_base64_run: 1000,
            display_tz: DisplayTz::UsEastern,
            allow_large_body: false,
            allow_fast_watch: false,
            allow_watch: false,
        }
    }
}

/// GitHub's documented ceilings that no configuration may exceed, per class:
/// `(per_minute, per_hour)`. `None` means GitHub documents no separate number.
///
/// * WRITE: "no more than 80 content-generating requests per minute and no more than 500
///   content-generating requests per hour" (secondary limits).
/// * READ: "5,000 requests per hour" (primary limit) and 900 points per minute for REST.
/// * SEARCH: "30 requests per minute for all search endpoints except ... code" (code: 10).
pub fn ceiling(class: Class) -> (Option<f64>, Option<u32>) {
    match class {
        Class::Write => (Some(80.0), Some(500)),
        Class::Read => (Some(900.0), Some(5000)),
        Class::Search => (Some(30.0), Some(1800)),
        Class::GitCredential | Class::Local => (None, None),
    }
}

impl Config {
    /// Limits for a class (LOCAL has none and returns the READ limits, which are never used).
    pub fn limits(&self, class: Class) -> ClassLimits {
        match class {
            Class::Read | Class::Local => self.read,
            Class::Search => self.search,
            Class::Write => self.write,
            Class::GitCredential => self.git_credential,
        }
    }

    /// Longest total wait for one call of `class`, in seconds, and the environment variable
    /// that sets it: `max_wait_secs` (`GH_PACED_MAX_WAIT`), except that GIT_CREDENTIAL uses the
    /// smaller of that and `git_credential_max_wait_secs` (`GH_PACED_GIT_MAX_WAIT`).
    pub fn max_wait(&self, class: Class) -> (f64, &'static str) {
        if class == Class::GitCredential && self.git_credential_max_wait_secs < self.max_wait_secs {
            (self.git_credential_max_wait_secs, "GH_PACED_GIT_MAX_WAIT")
        } else {
            (self.max_wait_secs, "GH_PACED_MAX_WAIT")
        }
    }

    fn limits_mut(&mut self, class: Class) -> &mut ClassLimits {
        match class {
            Class::Read | Class::Local => &mut self.read,
            Class::Search => &mut self.search,
            Class::Write => &mut self.write,
            Class::GitCredential => &mut self.git_credential,
        }
    }

    /// Load the effective configuration.
    ///
    /// `file` is the config file to read (`None` = defaults only; a path that does not exist is
    /// an error only when `explicit` is true). `env` looks up environment variables. Returns the
    /// configuration and the warnings to print.
    pub fn load(
        file: Option<&Path>,
        explicit: bool,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(Config, Vec<String>), String> {
        let mut config = Config::default();
        let mut warnings = Vec::new();
        if let Some(path) = file {
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    let parsed: FileConfig = serde_json::from_str(&text)
                        .map_err(|e| format!("config file {}: {e}", path.display()))?;
                    parsed.apply(&mut config, &mut warnings)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {}
                Err(e) => return Err(format!("config file {}: {e}", path.display())),
            }
        }
        apply_env(&mut config, env, &mut warnings)?;
        Ok((config, warnings))
    }
}

/// Where the config file lives: `$GH_PACED_CONFIG` (explicit), else
/// `$XDG_CONFIG_HOME/gh-paced/config.json`, else `~/.config/gh-paced/config.json`.
/// The boolean is true when the path was given explicitly.
pub fn config_path(env: &dyn Fn(&str) -> Option<String>) -> Option<(PathBuf, bool)> {
    if let Some(p) = env("GH_PACED_CONFIG").filter(|p| !p.is_empty()) {
        return Some((PathBuf::from(p), true));
    }
    if let Some(x) = env("XDG_CONFIG_HOME").filter(|p| p.starts_with('/')) {
        return Some((PathBuf::from(x).join("gh-paced").join("config.json"), false));
    }
    env("HOME")
        .filter(|h| h.starts_with('/'))
        .map(|h| (PathBuf::from(h).join(".config/gh-paced/config.json"), false))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassFile {
    per_minute: Option<f64>,
    burst: Option<f64>,
    per_hour: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFile {
    per_minute: Option<f64>,
    burst: Option<f64>,
    per_hour: Option<u32>,
    max_in_flight: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    read: Option<ClassFile>,
    search: Option<ClassFile>,
    write: Option<WriteFile>,
    git_credential: Option<ClassFile>,
    paginate_cost: Option<u32>,
    watch_cost: Option<u32>,
    min_watch_interval_secs: Option<f64>,
    max_wait_secs: Option<f64>,
    git_credential_max_wait_secs: Option<f64>,
    lock_wait_secs: Option<f64>,
    cooldown_secs: Option<f64>,
    plain_403_cooldown_secs: Option<f64>,
    rate_limit_refresh_secs: Option<f64>,
    rate_limit_refresh_calls: Option<u32>,
    rate_limit_min_refresh_secs: Option<f64>,
    rate_limit_timeout_secs: Option<f64>,
    halve_below_fraction: Option<f64>,
    block_below_fraction: Option<f64>,
    max_body_bytes: Option<usize>,
    max_base64_run: Option<usize>,
    display_tz: Option<DisplayTz>,
}

fn finite_at_least(name: &str, value: f64, min: f64) -> Result<f64, String> {
    if !value.is_finite() || value < min {
        return Err(format!(
            "{name} must be a finite number >= {min}, got {value}"
        ));
    }
    Ok(value)
}

fn finite_in(name: &str, value: f64, min: f64, max: f64) -> Result<f64, String> {
    if !value.is_finite() || value < min || value > max {
        return Err(format!(
            "{name} must be a finite number in {min}..={max}, got {value}"
        ));
    }
    Ok(value)
}

/// Floors and caps that protect the account whatever the configured class rates are: a config
/// file may make these stricter, never looser.
pub mod floors {
    /// Shortest cooldown after GitHub pushback, seconds (the requested 15 minutes).
    pub const MIN_COOLDOWN_SECS: f64 = 900.0;
    /// Highest account-wide fraction at which blocking may start (default and floor 20%).
    pub const MIN_BLOCK_FRACTION: f64 = 0.2;
    /// Highest account-wide fraction at which halving may start (default and floor 50%).
    pub const MIN_HALVE_FRACTION: f64 = 0.5;
    /// Largest write body without `GH_PACED_ALLOW_LARGE_BODY=1`, bytes.
    pub const MAX_BODY_BYTES: usize = 8192;
    /// Longest base64-looking run without `GH_PACED_ALLOW_LARGE_BODY=1`, characters.
    pub const MAX_BASE64_RUN: usize = 1000;
    /// Longest snapshot age before a refresh, seconds (the requested 5 minutes).
    pub const MAX_REFRESH_SECS: f64 = 300.0;
    /// Most admitted calls between refreshes (the requested 50).
    pub const MAX_REFRESH_CALLS: u32 = 50;
    /// Longest configurable duration (cooldowns, `max_wait_secs`, `GH_PACED_MAX_WAIT`,
    /// `git_credential_max_wait_secs`, `GH_PACED_GIT_MAX_WAIT`), seconds:
    /// 7 days. A longer value is a configuration error, so every deadline computed from one
    /// stays representable.
    pub const MAX_DURATION_SECS: f64 = 604_800.0;
    /// Longest `rate_limit_timeout_secs`, seconds.
    pub const MAX_RATE_LIMIT_TIMEOUT_SECS: f64 = 3600.0;
}

fn clamp_class(class: Class, limits: &mut ClassLimits, warnings: &mut Vec<String>) {
    let (per_minute_ceiling, per_hour_ceiling) = ceiling(class);
    if let Some(max) = per_minute_ceiling {
        if limits.per_minute > max {
            warnings.push(format!(
                "config: {} per_minute {} exceeds GitHub's documented {max}/min; using {max}",
                class.name(),
                limits.per_minute
            ));
            limits.per_minute = max;
        }
    }
    if let Some(max) = per_hour_ceiling {
        if limits.per_hour > max {
            warnings.push(format!(
                "config: {} per_hour {} exceeds GitHub's documented {max}/hour; using {max}",
                class.name(),
                limits.per_hour
            ));
            limits.per_hour = max;
        }
    }
    if limits.burst > limits.per_minute.max(1.0) * 10.0 {
        let max = limits.per_minute.max(1.0) * 10.0;
        warnings.push(format!(
            "config: {} burst {} is more than ten minutes of refill; using {max}",
            class.name(),
            limits.burst
        ));
        limits.burst = max;
    }
}

fn apply_class(
    class: Class,
    file: (Option<f64>, Option<f64>, Option<u32>),
    limits: &mut ClassLimits,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let name = class.name();
    if let Some(v) = file.0 {
        limits.per_minute = finite_at_least(&format!("{name}.per_minute"), v, 0.01)?;
    }
    if let Some(v) = file.1 {
        limits.burst = finite_at_least(&format!("{name}.burst"), v, 1.0)?;
    }
    if let Some(v) = file.2 {
        if v == 0 {
            return Err(format!("{name}.per_hour must be at least 1"));
        }
        limits.per_hour = v;
    }
    clamp_class(class, limits, warnings);
    Ok(())
}

impl FileConfig {
    fn apply(self, c: &mut Config, warnings: &mut Vec<String>) -> Result<(), String> {
        if let Some(f) = self.read {
            apply_class(
                Class::Read,
                (f.per_minute, f.burst, f.per_hour),
                &mut c.read,
                warnings,
            )?;
        }
        if let Some(f) = self.search {
            apply_class(
                Class::Search,
                (f.per_minute, f.burst, f.per_hour),
                &mut c.search,
                warnings,
            )?;
        }
        if let Some(f) = self.write {
            apply_class(
                Class::Write,
                (f.per_minute, f.burst, f.per_hour),
                &mut c.write,
                warnings,
            )?;
            if let Some(v) = f.max_in_flight {
                if !(1..=4).contains(&v) {
                    return Err(format!("write.max_in_flight must be 1..=4, got {v}"));
                }
                c.write_max_in_flight = v;
            }
        }
        if let Some(f) = self.git_credential {
            apply_class(
                Class::GitCredential,
                (f.per_minute, f.burst, f.per_hour),
                &mut c.git_credential,
                warnings,
            )?;
        }
        if let Some(v) = self.paginate_cost {
            c.paginate_cost = v.max(1);
        }
        if let Some(v) = self.watch_cost {
            c.watch_cost = v.max(1);
        }
        if let Some(v) = self.min_watch_interval_secs {
            c.min_watch_interval_secs =
                finite_in("min_watch_interval_secs", v, 0.0, floors::MAX_DURATION_SECS)?;
        }
        if let Some(v) = self.max_wait_secs {
            c.max_wait_secs = finite_in("max_wait_secs", v, 0.0, floors::MAX_DURATION_SECS)?;
        }
        if let Some(v) = self.git_credential_max_wait_secs {
            c.git_credential_max_wait_secs = finite_in(
                "git_credential_max_wait_secs",
                v,
                0.0,
                floors::MAX_DURATION_SECS,
            )?;
        }
        if let Some(v) = self.lock_wait_secs {
            c.lock_wait_secs = finite_in("lock_wait_secs", v, 0.1, 3600.0)?;
        }
        if let Some(v) = self.cooldown_secs {
            c.cooldown_secs = finite_in(
                "cooldown_secs",
                v,
                floors::MIN_COOLDOWN_SECS,
                floors::MAX_DURATION_SECS,
            )?;
        }
        if let Some(v) = self.plain_403_cooldown_secs {
            c.plain_403_cooldown_secs = finite_in(
                "plain_403_cooldown_secs",
                v,
                floors::MIN_COOLDOWN_SECS,
                floors::MAX_DURATION_SECS,
            )?;
        }
        if let Some(v) = self.rate_limit_refresh_secs {
            c.rate_limit_refresh_secs =
                finite_in("rate_limit_refresh_secs", v, 60.0, floors::MAX_REFRESH_SECS)?;
        }
        if let Some(v) = self.rate_limit_refresh_calls {
            if !(1..=floors::MAX_REFRESH_CALLS).contains(&v) {
                return Err(format!(
                    "rate_limit_refresh_calls must be in 1..={}, got {v}",
                    floors::MAX_REFRESH_CALLS
                ));
            }
            c.rate_limit_refresh_calls = v;
        }
        if let Some(v) = self.rate_limit_min_refresh_secs {
            c.rate_limit_min_refresh_secs =
                finite_in("rate_limit_min_refresh_secs", v, 10.0, 300.0)?;
        }
        if let Some(v) = self.rate_limit_timeout_secs {
            c.rate_limit_timeout_secs = finite_in(
                "rate_limit_timeout_secs",
                v,
                1.0,
                floors::MAX_RATE_LIMIT_TIMEOUT_SECS,
            )?;
        }
        if let Some(v) = self.block_below_fraction {
            c.block_below_fraction =
                finite_in("block_below_fraction", v, floors::MIN_BLOCK_FRACTION, 0.9)?;
        }
        if let Some(v) = self.halve_below_fraction {
            c.halve_below_fraction =
                finite_in("halve_below_fraction", v, floors::MIN_HALVE_FRACTION, 1.0)?;
        }
        if c.halve_below_fraction < c.block_below_fraction {
            return Err(format!(
                "halve_below_fraction ({}) must be >= block_below_fraction ({})",
                c.halve_below_fraction, c.block_below_fraction
            ));
        }
        if let Some(v) = self.max_body_bytes {
            if !(256..=floors::MAX_BODY_BYTES).contains(&v) {
                return Err(format!(
                    "max_body_bytes must be in 256..={}, got {v}",
                    floors::MAX_BODY_BYTES
                ));
            }
            c.max_body_bytes = v;
        }
        if let Some(v) = self.max_base64_run {
            if !(100..=floors::MAX_BASE64_RUN).contains(&v) {
                return Err(format!(
                    "max_base64_run must be in 100..={}, got {v}",
                    floors::MAX_BASE64_RUN
                ));
            }
            c.max_base64_run = v;
        }
        if let Some(v) = self.display_tz {
            c.display_tz = v;
        }
        Ok(())
    }
}

fn env_number(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Result<Option<f64>, String> {
    match env(name) {
        None => Ok(None),
        Some(raw) if raw.trim().is_empty() => Ok(None),
        Some(raw) => {
            let value: f64 = raw
                .trim()
                .parse()
                .map_err(|_| format!("{name}={raw:?} is not a number"))?;
            if !value.is_finite() || value < 0.0 {
                return Err(format!(
                    "{name}={raw:?} must be a finite non-negative number"
                ));
            }
            Ok(Some(value))
        }
    }
}

fn env_flag(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Result<bool, String> {
    match env(name).as_deref().map(str::trim) {
        None | Some("") | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(format!("{name}={other:?} must be 1 or 0")),
    }
}

/// Apply environment overrides. Rate overrides may only TIGHTEN: a value looser than the
/// configured one is ignored with a warning, so an exported variable cannot raise a budget.
fn apply_env(
    c: &mut Config,
    env: &dyn Fn(&str) -> Option<String>,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    for (class, prefix) in [
        (Class::Read, "GH_PACED_READ"),
        (Class::Search, "GH_PACED_SEARCH"),
        (Class::Write, "GH_PACED_WRITE"),
        (Class::GitCredential, "GH_PACED_GIT"),
    ] {
        let limits = c.limits_mut(class);
        if let Some(v) = env_number(env, &format!("{prefix}_PER_MINUTE"))? {
            if v <= 0.0 {
                return Err(format!("{prefix}_PER_MINUTE must be greater than 0"));
            }
            if v <= limits.per_minute {
                limits.per_minute = v;
            } else {
                warnings.push(format!(
                    "{prefix}_PER_MINUTE={v} is looser than the configured {}; ignored (environment overrides only tighten)",
                    limits.per_minute
                ));
            }
        }
        if let Some(v) = env_number(env, &format!("{prefix}_BURST"))? {
            if v < 1.0 {
                return Err(format!("{prefix}_BURST must be at least 1"));
            }
            if v <= limits.burst {
                limits.burst = v;
            } else {
                warnings.push(format!(
                    "{prefix}_BURST={v} is looser than the configured {}; ignored (environment overrides only tighten)",
                    limits.burst
                ));
            }
        }
        if let Some(v) = env_number(env, &format!("{prefix}_PER_HOUR"))? {
            if v < 1.0 {
                return Err(format!("{prefix}_PER_HOUR must be at least 1"));
            }
            let v = v.floor() as u32;
            if v <= limits.per_hour {
                limits.per_hour = v;
            } else {
                warnings.push(format!(
                    "{prefix}_PER_HOUR={v} is looser than the configured {}; ignored (environment overrides only tighten)",
                    limits.per_hour
                ));
            }
        }
    }
    if let Some(v) = env_number(env, "GH_PACED_PAGINATE_COST")? {
        let v = v.ceil() as u32;
        if v >= c.paginate_cost {
            c.paginate_cost = v;
        } else {
            warnings.push(format!(
                "GH_PACED_PAGINATE_COST={v} is cheaper than the configured {}; ignored (environment overrides only tighten)",
                c.paginate_cost
            ));
        }
    }
    if let Some(v) = env_number(env, "GH_PACED_MAX_WAIT")? {
        if v > floors::MAX_DURATION_SECS {
            return Err(format!(
                "GH_PACED_MAX_WAIT={v} must be at most {} seconds",
                floors::MAX_DURATION_SECS
            ));
        }
        c.max_wait_secs = v;
    }
    if let Some(v) = env_number(env, "GH_PACED_GIT_MAX_WAIT")? {
        if v > floors::MAX_DURATION_SECS {
            return Err(format!(
                "GH_PACED_GIT_MAX_WAIT={v} must be at most {} seconds",
                floors::MAX_DURATION_SECS
            ));
        }
        c.git_credential_max_wait_secs = v;
    }
    if let Some(v) = env_number(env, "GH_PACED_LOCK_WAIT")? {
        if !(0.1..=3600.0).contains(&v) {
            return Err(format!(
                "GH_PACED_LOCK_WAIT={v} must be in 0.1..=3600 seconds"
            ));
        }
        c.lock_wait_secs = v;
    }
    match env("GH_PACED_DISPLAY_TZ").as_deref().map(str::trim) {
        None | Some("") => {}
        Some("US-Eastern") => c.display_tz = DisplayTz::UsEastern,
        Some("UTC") => c.display_tz = DisplayTz::Utc,
        Some(other) => {
            return Err(format!(
                "GH_PACED_DISPLAY_TZ={other:?} must be US-Eastern or UTC"
            ))
        }
    }
    c.allow_large_body = env_flag(env, "GH_PACED_ALLOW_LARGE_BODY")?;
    c.allow_fast_watch = env_flag(env, "GH_PACED_ALLOW_FAST_WATCH")?;
    c.allow_watch = env_flag(env, "GH_PACED_ALLOW_WATCH")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn defaults_match_the_documented_budgets() {
        let c = Config::default();
        assert_eq!(
            (c.read.per_minute, c.read.burst, c.read.per_hour),
            (20.0, 10.0, 500)
        );
        assert_eq!(c.search.per_minute, 5.0);
        assert_eq!(
            (c.write.per_minute, c.write.burst, c.write.per_hour),
            (2.0, 1.0, 30)
        );
        assert_eq!(c.write_max_in_flight, 1);
        assert_eq!(
            (c.git_credential.per_minute, c.git_credential.per_hour),
            (6.0, 120)
        );
        assert_eq!(c.max_wait_secs, 900.0);
        assert_eq!(c.cooldown_secs, 900.0);
        assert_eq!(c.max_body_bytes, 8192);
        assert_eq!(c.max_base64_run, 1000);
        assert_eq!(c.paginate_cost, 10);
    }

    #[test]
    fn env_overrides_only_tighten() {
        let env = env_of(&[
            ("GH_PACED_WRITE_PER_HOUR", "10"),
            ("GH_PACED_READ_PER_MINUTE", "99"),
            ("GH_PACED_PAGINATE_COST", "3"),
            ("GH_PACED_MAX_WAIT", "5"),
        ]);
        let (c, warnings) = Config::load(None, false, &env).unwrap();
        assert_eq!(c.write.per_hour, 10);
        assert_eq!(c.read.per_minute, 20.0, "looser read rate is ignored");
        assert_eq!(c.paginate_cost, 10, "cheaper paginate cost is ignored");
        assert_eq!(c.max_wait_secs, 5.0);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn malformed_env_is_an_error_not_a_silent_default() {
        let env = env_of(&[("GH_PACED_WRITE_PER_HOUR", "lots")]);
        assert!(Config::load(None, false, &env).is_err());
        let env = env_of(&[("GH_PACED_ALLOW_LARGE_BODY", "yes")]);
        assert!(Config::load(None, false, &env).is_err());
    }

    #[test]
    fn config_file_is_clamped_to_github_ceilings_and_rejects_unknown_keys() {
        let dir = std::env::temp_dir().join(format!("gh-paced-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.json");
        std::fs::write(
            &path,
            r#"{"write": {"per_minute": 500, "per_hour": 9000, "max_in_flight": 1}, "search": {"per_minute": 4}}"#,
        )
        .unwrap();
        let env = env_of(&[]);
        let (c, warnings) = Config::load(Some(&path), true, &env).unwrap();
        assert_eq!(c.write.per_minute, 80.0);
        assert_eq!(c.write.per_hour, 500);
        assert_eq!(c.search.per_minute, 4.0);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        std::fs::write(&path, r#"{"writes": {"per_minute": 1}}"#).unwrap();
        assert!(Config::load(Some(&path), true, &env).is_err());
        std::fs::write(&path, r#"{"cooldown_secs": 5}"#).unwrap();
        assert!(Config::load(Some(&path), true, &env).is_err());
        std::fs::write(&path, r#"{"cooldown_secs": 1200}"#).unwrap();
        assert_eq!(
            Config::load(Some(&path), true, &env)
                .unwrap()
                .0
                .cooldown_secs,
            1200.0
        );
        let missing = dir.join("missing.json");
        assert!(Config::load(Some(&missing), true, &env).is_err());
        assert!(Config::load(Some(&missing), false, &env).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The pushback cooldowns, the account-wide halve/block thresholds, the refresh cadence and
    /// the content limits protect the account whatever the class rates are, so a config file can
    /// only make them stricter.
    #[test]
    fn config_file_cannot_weaken_mandatory_protections() {
        let dir = std::env::temp_dir().join(format!("gh-paced-floors-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.json");
        let env = env_of(&[]);
        let load = |json: &str| {
            std::fs::write(&path, json).unwrap();
            Config::load(Some(&path), true, &env)
        };
        for weaker in [
            r#"{"cooldown_secs": 60}"#,
            r#"{"cooldown_secs": 899}"#,
            r#"{"plain_403_cooldown_secs": 0}"#,
            r#"{"plain_403_cooldown_secs": 120}"#,
            r#"{"block_below_fraction": 0.05}"#,
            r#"{"block_below_fraction": 0.19}"#,
            r#"{"halve_below_fraction": 0.3, "block_below_fraction": 0.2}"#,
            r#"{"max_body_bytes": 8193}"#,
            r#"{"max_body_bytes": 65536}"#,
            r#"{"max_base64_run": 1001}"#,
            r#"{"rate_limit_refresh_secs": 301}"#,
            r#"{"rate_limit_refresh_calls": 51}"#,
            r#"{"rate_limit_refresh_calls": 0}"#,
            r#"{"rate_limit_min_refresh_secs": 3600}"#,
        ] {
            assert!(load(weaker).is_err(), "accepted {weaker}");
        }
        for stricter in [
            r#"{"cooldown_secs": 3600}"#,
            r#"{"plain_403_cooldown_secs": 1800}"#,
            r#"{"block_below_fraction": 0.4, "halve_below_fraction": 0.8}"#,
            r#"{"max_body_bytes": 4096, "max_base64_run": 200}"#,
            r#"{"rate_limit_refresh_secs": 120, "rate_limit_refresh_calls": 10}"#,
        ] {
            assert!(load(stricter).is_ok(), "rejected {stricter}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A duration too long to turn into a deadline is a configuration error, never a panic
    /// later (a 1e308 refresh timeout used to panic in `Duration::from_secs_f64`).
    #[test]
    fn durations_must_be_representable() {
        let dir = std::env::temp_dir().join(format!("gh-paced-durations-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.json");
        let env = env_of(&[]);
        let load = |json: &str| {
            std::fs::write(&path, json).unwrap();
            Config::load(Some(&path), true, &env)
        };
        for key in [
            "cooldown_secs",
            "plain_403_cooldown_secs",
            "rate_limit_timeout_secs",
            "max_wait_secs",
            "min_watch_interval_secs",
        ] {
            for value in ["1e308", "604801"] {
                let json = format!("{{\"{key}\": {value}}}");
                assert!(load(&json).is_err(), "accepted {json}");
            }
        }
        assert!(load(r#"{"rate_limit_timeout_secs": 3601}"#).is_err());
        for ok in [
            r#"{"cooldown_secs": 604800}"#,
            r#"{"max_wait_secs": 604800}"#,
            r#"{"rate_limit_timeout_secs": 3600}"#,
        ] {
            assert!(load(ok).is_ok(), "rejected {ok}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
        for v in ["1e308", "604801"] {
            let env = env_of(&[("GH_PACED_MAX_WAIT", v)]);
            assert!(Config::load(None, false, &env).is_err(), "accepted {v}");
        }
        let env = env_of(&[("GH_PACED_MAX_WAIT", "604800")]);
        assert!(Config::load(None, false, &env).is_ok());
    }

    #[test]
    fn git_credential_wait_has_its_own_short_bound() {
        let (c, warnings) = Config::load(None, false, &env_of(&[])).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(c.git_credential_max_wait_secs, 30.0);
        assert_eq!(
            c.max_wait(Class::GitCredential),
            (30.0, "GH_PACED_GIT_MAX_WAIT")
        );
        for class in [Class::Read, Class::Search, Class::Write] {
            assert_eq!(c.max_wait(class), (900.0, "GH_PACED_MAX_WAIT"), "{class:?}");
        }
        // The smaller of the two applies to GIT_CREDENTIAL; the other classes never use it.
        let env = env_of(&[("GH_PACED_GIT_MAX_WAIT", "5")]);
        let (c, _) = Config::load(None, false, &env).unwrap();
        assert_eq!(
            c.max_wait(Class::GitCredential),
            (5.0, "GH_PACED_GIT_MAX_WAIT")
        );
        assert_eq!(c.max_wait(Class::Read), (900.0, "GH_PACED_MAX_WAIT"));
        let env = env_of(&[("GH_PACED_MAX_WAIT", "10")]);
        let (c, _) = Config::load(None, false, &env).unwrap();
        assert_eq!(
            c.max_wait(Class::GitCredential),
            (10.0, "GH_PACED_MAX_WAIT")
        );
        let env = env_of(&[
            ("GH_PACED_MAX_WAIT", "10"),
            ("GH_PACED_GIT_MAX_WAIT", "600"),
        ]);
        let (c, _) = Config::load(None, false, &env).unwrap();
        assert_eq!(
            c.max_wait(Class::GitCredential),
            (10.0, "GH_PACED_MAX_WAIT")
        );
        for bad in ["-1", "lots", "inf", "604801"] {
            let env = env_of(&[("GH_PACED_GIT_MAX_WAIT", bad)]);
            assert!(Config::load(None, false, &env).is_err(), "accepted {bad}");
        }
        let env = env_of(&[("GH_PACED_GIT_MAX_WAIT", "0")]);
        let (c, _) = Config::load(None, false, &env).unwrap();
        assert_eq!(c.git_credential_max_wait_secs, 0.0);
        // The config file key, range-checked like max_wait_secs.
        let dir = std::env::temp_dir().join(format!("gh-paced-gitwait-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.json");
        let load = |json: &str| {
            std::fs::write(&path, json).unwrap();
            Config::load(Some(&path), true, &env_of(&[]))
        };
        let (c, _) = load(r#"{"git_credential_max_wait_secs": 12}"#).unwrap();
        assert_eq!(c.git_credential_max_wait_secs, 12.0);
        for bad in ["-1", "604801", "1e308"] {
            let json = format!("{{\"git_credential_max_wait_secs\": {bad}}}");
            assert!(load(&json).is_err(), "accepted {json}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
