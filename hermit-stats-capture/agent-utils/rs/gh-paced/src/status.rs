//! `gh-paced status`: a read-only view of one account's (or every account's) pacing state.
//!
//! Status takes no lock and creates nothing: it reads the state file (which is always replaced
//! whole, by rename) and the audit log, probes lease files for liveness, never saves, never
//! refreshes the GitHub snapshot, and never contacts GitHub.

use crate::audit;
use crate::budget::{halved, Bucket};
use crate::classify::Class;
use crate::config::Config;
use crate::pushback::has_no_end;
use crate::state::{self, Paths, State};
use crate::timefmt::human;
use serde_json::{json, Value};
use std::path::Path;

/// Audit records shown by `status`.
pub const RECENT: usize = 5;

/// Build the status report for one account at `now`.
pub fn report(paths: &Paths, cfg: &Config, now: f64) -> Result<Value, String> {
    let st: State = state::load_readonly(paths)?;
    let pressure_of = |class: Class| st.rate_limit.as_ref().and_then(|s| s.pressure(class, now));
    let mut classes = serde_json::Map::new();
    for class in Class::PACED {
        let base = cfg.limits(class);
        let halve = matches!(class, Class::Read | Class::Search | Class::Write)
            && pressure_of(class).is_some_and(|p| p.fraction <= cfg.halve_below_fraction);
        let limits = if halve { halved(base) } else { base };
        let mut b = st
            .buckets
            .get(class.name())
            .cloned()
            .unwrap_or_else(|| Bucket::full(base, now));
        b.refresh(limits, now);
        classes.insert(
            class.name().to_string(),
            json!({
                "level": (b.level * 100.0).round() / 100.0,
                "burst": limits.burst,
                "per_minute": limits.per_minute,
                "hour_used": b.hour_used(),
                "per_hour": limits.per_hour,
                "halved": halve,
                "blocked_until": b.blocked_until.filter(|t| *t > now).map(|t| human(t, now, cfg.display_tz)),
                "next_slot_secs": (b.bucket_wait(limits, 1).max(b.hour_wait(limits, 1, now)) * 10.0).ceil() / 10.0,
            }),
        );
    }
    let in_flight: Vec<Value> = st
        .in_flight
        .iter()
        .map(|h| {
            json!({
                "pid": h.pid,
                "class": h.class.name(),
                "since_secs": (now - h.since).max(0.0).round(),
                "alive": state::holder_alive_in(paths, h),
            })
        })
        .collect();
    let cooldown = st.cooldown.as_ref().filter(|c| c.until > now).map(|c| {
        let no_end = has_no_end(c.until);
        json!({
            "remaining_secs": if no_end { Value::Null } else { json!((c.until - now).ceil()) },
            "until": if no_end {
                "no end time".to_string()
            } else {
                human(c.until, now, cfg.display_tz)
            },
            "reason": c.reason,
            "command": c.command,
        })
    });
    let snapshot = st.rate_limit.as_ref().map(|s| {
        let resources: serde_json::Map<String, Value> = s
            .resources
            .iter()
            .filter(|(k, _)| matches!(k.as_str(), "core" | "graphql" | "search"))
            .map(|(k, r)| {
                (
                    k.clone(),
                    json!({"remaining": r.remaining, "limit": r.limit,
                           "reset": human(r.reset, now, cfg.display_tz)}),
                )
            })
            .collect();
        json!({
            "age_secs": (now - s.fetched_at).max(0.0).round(),
            "resources": resources,
        })
    });
    Ok(json!({
        "account": paths.account,
        "state_file": paths.state().display().to_string(),
        "classes": classes,
        "in_flight": in_flight,
        "cooldown": cooldown,
        "rate_limit": snapshot,
        "calls_since_refresh": st.calls_since_refresh,
        "recent": audit::tail(&paths.audit(), RECENT),
    }))
}

/// Render a report as text.
pub fn render(v: &Value) -> String {
    let mut out = String::new();
    let s = |v: &Value| {
        v.as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string())
    };
    out.push_str(&format!(
        "gh-paced status for account {}\n",
        s(&v["account"])
    ));
    out.push_str(&format!("  state file: {}\n", s(&v["state_file"])));
    if let Some(cd) = v["cooldown"]
        .as_object()
        .filter(|cd| cd["remaining_secs"].is_null())
    {
        out.push_str(&format!(
            "  COOLDOWN: no end time, after {} on `{}`; every paced call is refused until a \
             person removes the `.cooldown` file beside the state file and the state file's \
             `cooldown` entry\n",
            s(&cd["reason"]),
            s(&cd["command"])
        ));
    } else if let Some(cd) = v["cooldown"].as_object() {
        out.push_str(&format!(
            "  COOLDOWN: {} s left (until {}) after {} on `{}`\n",
            cd["remaining_secs"],
            s(&cd["until"]),
            s(&cd["reason"]),
            s(&cd["command"])
        ));
    } else {
        out.push_str("  cooldown: none\n");
    }
    // `serde_json::Value`'s Display ignores width and alignment, so render each number to a
    // String first; otherwise the columns do not line up.
    out.push_str("  class           tokens/burst   rate        last hour  next slot\n");
    if let Some(classes) = v["classes"].as_object() {
        for (name, c) in classes {
            out.push_str(&format!(
                "  {:<15} {:>6}/{:<6}  {:>6}/min  {:>4}/{:<4}  {}{}\n",
                name,
                c["level"].to_string(),
                c["burst"].to_string(),
                c["per_minute"].to_string(),
                c["hour_used"].to_string(),
                c["per_hour"].to_string(),
                if c["next_slot_secs"].as_f64().unwrap_or(0.0) > 0.0 {
                    format!("in {} s", c["next_slot_secs"])
                } else {
                    "now".to_string()
                },
                if c["halved"].as_bool().unwrap_or(false) {
                    "  (halved)".to_string()
                } else if let Some(t) = c["blocked_until"].as_str() {
                    format!("  (blocked after state recovery until {t})")
                } else {
                    String::new()
                }
            ));
        }
    }
    match v["in_flight"].as_array() {
        Some(a) if !a.is_empty() => {
            for h in a {
                out.push_str(&format!(
                    "  in flight: pid {} {} for {} s{}\n",
                    h["pid"],
                    s(&h["class"]),
                    h["since_secs"],
                    if h["alive"].as_bool().unwrap_or(false) {
                        ""
                    } else {
                        " (exited; reaped on next call)"
                    }
                ));
            }
        }
        _ => out.push_str("  in flight: none\n"),
    }
    if let Some(rl) = v["rate_limit"].as_object() {
        out.push_str(&format!("  GitHub snapshot: {} s old\n", rl["age_secs"]));
        if let Some(res) = rl["resources"].as_object() {
            for (k, r) in res {
                out.push_str(&format!(
                    "    {:<8} {}/{} left, resets {}\n",
                    k,
                    r["remaining"],
                    r["limit"],
                    s(&r["reset"])
                ));
            }
        }
    } else {
        out.push_str("  GitHub snapshot: none yet (fetched before the next API call)\n");
    }
    out.push_str(&format!(
        "  calls since snapshot: {}\n",
        v["calls_since_refresh"]
    ));
    if let Some(recent) = v["recent"].as_array() {
        if !recent.is_empty() {
            out.push_str("  recent audit records:\n");
            for r in recent {
                let mut line = format!(
                    "    {} {:<8} {:<10} {}",
                    s(&r["et"]),
                    s(&r["event"]),
                    s(&r["class"]),
                    s(&r["command"])
                );
                if let Some(rc) = r.get("rc") {
                    line.push_str(&format!(" rc={rc}"));
                }
                if let Some(d) = r.get("detail").and_then(Value::as_str) {
                    line.push_str(&format!(" ({d})"));
                }
                out.push_str(&line);
                out.push('\n');
            }
        }
    }
    out
}

/// Accounts with a state file in `dir`, sorted.
pub fn accounts(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let account = name.strip_suffix(".json")?.to_string();
            state::valid_account(&account).then_some(account)
        })
        .collect();
    out.sort();
    out
}
