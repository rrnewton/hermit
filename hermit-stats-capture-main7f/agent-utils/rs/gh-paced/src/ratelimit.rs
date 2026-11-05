//! The account-wide rate-limit snapshot from `GET /rate_limit`.
//!
//! GitHub documents that calling `GET /rate_limit` does not count against the primary REST
//! limit (it can count against secondary limits, so gh-paced still charges it to the read
//! bucket). The snapshot tells every host how much of the ACCOUNT's hourly allowance is left,
//! which the per-host budgets cannot know on their own: other hosts, other tools, and GitHub
//! Actions all draw on the same pool.

use crate::classify::Class;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One resource's numbers from `GET /rate_limit`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    /// Requests allowed per window.
    pub limit: u64,
    /// Requests left in the current window.
    pub remaining: u64,
    /// When the window resets (Unix seconds).
    pub reset: f64,
}

/// A stored `GET /rate_limit` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// When it was fetched (Unix seconds).
    pub fetched_at: f64,
    /// Resources by name (`core`, `graphql`, `search`, ...).
    pub resources: BTreeMap<String, Resource>,
}

/// A snapshot older than this is ignored.
pub const MAX_SNAPSHOT_AGE: f64 = 3600.0;

/// Parse the JSON body of `GET /rate_limit`.
pub fn parse(body: &[u8], fetched_at: f64) -> Result<Snapshot, String> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("rate_limit output is not JSON: {e}"))?;
    let resources = value
        .get("resources")
        .and_then(|r| r.as_object())
        .ok_or_else(|| "rate_limit output has no resources object".to_string())?;
    let mut out = BTreeMap::new();
    for (name, r) in resources {
        let field = |k: &str| r.get(k).and_then(serde_json::Value::as_u64);
        if let (Some(limit), Some(remaining), Some(reset)) =
            (field("limit"), field("remaining"), field("reset"))
        {
            out.insert(
                name.clone(),
                Resource {
                    limit,
                    remaining,
                    reset: reset as f64,
                },
            );
        }
    }
    if !out.contains_key("core") {
        return Err("rate_limit output has no core resource".to_string());
    }
    Ok(Snapshot {
        fetched_at,
        resources: out,
    })
}

/// The tightest relevant resource for a class: `(name, fraction remaining, reset time)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pressure {
    /// Resource name.
    pub resource: String,
    /// Remaining divided by limit, 0.0..=1.0.
    pub fraction: f64,
    /// Remaining requests.
    pub remaining: u64,
    /// Limit.
    pub limit: u64,
    /// Reset time (Unix seconds).
    pub reset: f64,
}

/// Resources that bound a class. GIT_CREDENTIAL and LOCAL are not bound by the API pools.
pub fn resources_for(class: Class) -> &'static [&'static str] {
    match class {
        Class::Read | Class::Write => &["core", "graphql"],
        Class::Search => &["core", "graphql", "search"],
        Class::GitCredential | Class::Local => &[],
    }
}

impl Snapshot {
    /// The tightest resource for `class` at `now`, or `None` when the snapshot is too old or no
    /// resource applies. A resource whose reset time has passed counts as full.
    pub fn pressure(&self, class: Class, now: f64) -> Option<Pressure> {
        if now - self.fetched_at > MAX_SNAPSHOT_AGE {
            return None;
        }
        let mut best: Option<Pressure> = None;
        for name in resources_for(class) {
            let Some(r) = self.resources.get(*name) else {
                continue;
            };
            let fraction = if r.reset <= now || r.limit == 0 {
                1.0
            } else {
                r.remaining as f64 / r.limit as f64
            };
            if best.as_ref().is_none_or(|b| fraction < b.fraction) {
                best = Some(Pressure {
                    resource: (*name).to_string(),
                    fraction,
                    remaining: r.remaining,
                    limit: r.limit,
                    reset: r.reset,
                });
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"resources":{
        "core":{"limit":5000,"used":4100,"remaining":900,"reset":600},
        "search":{"limit":30,"used":3,"remaining":27,"reset":700},
        "graphql":{"limit":5000,"used":100,"remaining":4900,"reset":1000}},
        "rate":{"limit":5000,"used":4100,"remaining":900,"reset":600}}"#;

    #[test]
    fn parse_and_pressure() {
        let s = parse(SAMPLE.as_bytes(), 100.0).expect("parse");
        assert_eq!(s.resources["core"].remaining, 900);
        let p = s.pressure(Class::Read, 200.0).expect("pressure");
        assert_eq!(p.resource, "core");
        assert!((p.fraction - 0.18).abs() < 1e-9);
        let p = s.pressure(Class::Search, 200.0).expect("pressure");
        assert_eq!(p.resource, "core");
        assert!(s.pressure(Class::GitCredential, 200.0).is_none());
        // After core resets (t=600) it counts as full; for SEARCH the search pool (27/30) is then
        // tightest until it resets at 700, while READ ignores search and sees graphql (0.98).
        let p = s.pressure(Class::Search, 650.0).expect("pressure");
        assert_eq!(p.resource, "search");
        assert!((p.fraction - 0.9).abs() < 1e-9);
        let p = s.pressure(Class::Read, 650.0).expect("pressure");
        assert_eq!(p.resource, "graphql");
        let p = s.pressure(Class::Read, 1000.0).expect("pressure");
        assert_eq!(p.fraction, 1.0);
        // Stale snapshots are ignored.
        assert!(s.pressure(Class::Read, 100.0 + 3601.0).is_none());
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse(b"not json", 0.0).is_err());
        assert!(parse(b"{}", 0.0).is_err());
        assert!(parse(
            br#"{"resources":{"search":{"limit":1,"remaining":1,"reset":1}}}"#,
            0.0
        )
        .is_err());
    }
}
