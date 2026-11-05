//! Deterministic, non-executing pressure-plan construction.

use serde::Serialize;

use crate::policy::{PolicyDecision, Verdict};

/// A bounded list of digest-bound candidates and all non-eligible decisions.
#[derive(Debug, Serialize)]
pub(crate) struct PressurePlan {
    pub(crate) schema: u64,
    #[serde(serialize_with = "crate::config::decimal_u64::serialize")]
    pub(crate) target_bytes: u64,
    #[serde(serialize_with = "crate::config::decimal_u128::serialize")]
    pub(crate) selected_reclaimable_bytes: u128,
    pub(crate) target_met: bool,
    pub(crate) eligible: Vec<PolicyDecision>,
    pub(crate) deferred_eligible: Vec<PolicyDecision>,
    pub(crate) blocked: Vec<PolicyDecision>,
    pub(crate) unknown: Vec<PolicyDecision>,
}

pub(crate) fn build(
    mut decisions: Vec<PolicyDecision>,
    target_bytes: u64,
    limit: usize,
) -> PressurePlan {
    decisions.sort_by(|left, right| {
        heartbeat_key(left.heartbeat_at.as_deref(), right.heartbeat_at.as_deref())
            .then_with(|| right.reclaimable_bytes.cmp(&left.reclaimable_bytes))
            .then_with(|| left.slot.cmp(&right.slot))
    });
    let mut eligible = Vec::new();
    let mut blocked = Vec::new();
    let mut unknown = Vec::new();
    let mut deferred_eligible = Vec::new();
    let mut selected_reclaimable_bytes = 0_u128;
    for decision in decisions {
        match decision.verdict {
            Verdict::Eligible
                if eligible.len() < limit
                    && (target_bytes == 0
                        || selected_reclaimable_bytes < u128::from(target_bytes)) =>
            {
                selected_reclaimable_bytes += u128::from(decision.reclaimable_bytes);
                eligible.push(decision);
            }
            Verdict::Eligible => deferred_eligible.push(decision),
            Verdict::Blocked => blocked.push(decision),
            Verdict::Unknown => unknown.push(decision),
        }
    }
    PressurePlan {
        schema: 1,
        target_bytes,
        selected_reclaimable_bytes,
        target_met: target_bytes == 0 || selected_reclaimable_bytes >= u128::from(target_bytes),
        eligible,
        deferred_eligible,
        blocked,
        unknown,
    }
}

fn heartbeat_key(left: Option<&str>, right: Option<&str>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => match (
            chrono::DateTime::parse_from_rfc3339(left),
            chrono::DateTime::parse_from_rfc3339(right),
        ) {
            (Ok(left), Ok(right)) => left.cmp(&right),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => left.cmp(right),
        },
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}
