//! Deterministic JSON/YAML fixture host for tests and offline demonstrations.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::host::VcsHost;
use crate::model::{
    edge_key, CheckRun, RawPr, ReviewEvidenceEvent, ReviewEvidenceSnapshot, DEFAULT_BASE,
    NATIVE_REVIEW_STATES,
};

#[derive(Clone, Debug)]
struct FakePr {
    raw: RawPr,
    head_sha: String,
    fetched_head_sha: String,
    changed_files: BTreeSet<String>,
    base_conflict_paths: Vec<String>,
    commits_behind: i64,
}

/// In-memory [`VcsHost`] backed by a validated JSON or YAML fixture.
pub struct FakeHost {
    prs: Vec<FakePr>,
    conflicts: BTreeMap<(i64, i64), Vec<String>>,
    ancestry: BTreeSet<(i64, i64)>,
    base_shas: BTreeMap<String, String>,
    by_number: BTreeMap<i64, usize>,
    number_by_sha: BTreeMap<String, i64>,
    base_ref_by_sha: BTreeMap<String, String>,
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueValueVisitor)
    }
}

struct UniqueValueVisitor;

impl<'de> Visitor<'de> for UniqueValueVisitor {
    type Value = UniqueValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON/YAML value with unique string mapping keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(UniqueValue)
            .ok_or_else(|| E::custom("non-finite numbers are not supported"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<UniqueValue>()? {
            values.push(value.0);
        }
        Ok(UniqueValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if key == "<<" {
                return Err(de::Error::custom("merge keys are not supported"));
            }
            if values.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate key {key:?}")));
            }
            let value = map.next_value::<UniqueValue>()?;
            values.insert(key, value.0);
        }
        Ok(UniqueValue(Value::Object(values)))
    }
}

fn object(value: &Value, where_: &str) -> Result<Map<String, Value>, String> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| format!("{where_}: expected an object, got non-object"))
}

fn opt_string(obj: &Map<String, Value>, key: &str, default: &str) -> String {
    match obj.get(key) {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        _ => default.to_owned(),
    }
}

fn required_review_string(
    obj: &Map<String, Value>,
    key: &str,
    where_: &str,
    allow_empty: bool,
) -> Result<String, String> {
    let value = obj
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{where_}: field {key:?} must be a string"))?;
    if !allow_empty && value.is_empty() {
        return Err(format!("{where_}: field {key:?} must be non-empty"));
    }
    Ok(value.to_owned())
}

fn optional_review_author(
    obj: &Map<String, Value>,
    key: &str,
    where_: &str,
) -> Result<String, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.trim().to_owned()),
        Some(_) => Err(format!("{where_}: field {key:?} must be a string or null")),
    }
}

fn required_nullable_review_string(
    obj: &Map<String, Value>,
    key: &str,
    where_: &str,
) -> Result<String, String> {
    match obj.get(key) {
        None => Err(format!("{where_}: field {key:?} is required")),
        Some(Value::Null) => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{where_}: field {key:?} must be a string or null")),
    }
}

fn required_integer(
    obj: &Map<String, Value>,
    key: &str,
    where_: &str,
    positive: bool,
) -> Result<i64, String> {
    let value = obj
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{where_}: field {key:?} must be an integer"))?;
    if positive && value <= 0 {
        return Err(format!(
            "{where_}: field {key:?} must be a positive integer"
        ));
    }
    Ok(value)
}

fn opt_integer(
    obj: &Map<String, Value>,
    key: &str,
    default: i64,
    where_: &str,
    nonnegative: bool,
) -> Result<i64, String> {
    let Some(raw) = obj.get(key) else {
        return Ok(default);
    };
    if raw.is_null() {
        return Ok(default);
    }
    let value = raw
        .as_i64()
        .ok_or_else(|| format!("{where_}: field {key:?} must be an integer"))?;
    if nonnegative && value < 0 {
        return Err(format!("{where_}: field {key:?} must be nonnegative"));
    }
    Ok(value)
}

fn strings(obj: &Map<String, Value>, key: &str) -> Vec<String> {
    obj.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|value| match value {
            Value::String(value) => value.clone(),
            Value::Bool(true) => "True".to_owned(),
            Value::Bool(false) => "False".to_owned(),
            Value::Null => "None".to_owned(),
            other => other.to_string().trim_matches('"').to_owned(),
        })
        .collect()
}

fn checks(value: Option<&Value>, where_: &str) -> Result<Vec<CheckRun>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let entries = value
        .as_array()
        .ok_or_else(|| format!("{where_}: 'checks' must be a list"))?;
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let obj = object(entry, &format!("{where_}.checks[{index}]"))?;
            let duration_secs = obj
                .get("duration_secs")
                .filter(|value| !value.is_null())
                .map(|_| opt_integer(&obj, "duration_secs", 0, where_, true))
                .transpose()?;
            Ok(CheckRun {
                name: opt_string(&obj, "name", ""),
                status: opt_string(&obj, "status", "").to_ascii_uppercase(),
                conclusion: opt_string(&obj, "conclusion", "").to_ascii_uppercase(),
                text: opt_string(&obj, "text", ""),
                workflow: opt_string(&obj, "workflow", ""),
                duration_secs,
            })
        })
        .collect()
}

fn review_snapshot(
    obj: &Map<String, Value>,
    where_: &str,
    default_head: &str,
    default_decision: &str,
) -> Result<Option<ReviewEvidenceSnapshot>, String> {
    let Some(raw_events) = obj.get("review_events") else {
        return Ok(None);
    };
    let entries = raw_events
        .as_array()
        .ok_or_else(|| format!("{where_}: 'review_events' must be a list"))?;
    let mut events = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let role = format!("{where_}.review_events[{index}]");
        let event = object(entry, &role)?;
        let kind = required_review_string(&event, "kind", &role, false)?;
        let identity = required_review_string(&event, "identity", &role, false)?;
        let author = optional_review_author(&event, "author", &role)?;
        let state = required_review_string(&event, "state", &role, false)?;
        if kind == "review" && !NATIVE_REVIEW_STATES.contains(&state.as_str()) {
            return Err(format!("{role}: review has unknown state {state:?}"));
        }
        let head_sha = required_review_string(&event, "head_sha", &role, true)?;
        if kind == "review" && head_sha.is_empty() {
            return Err(format!("{role}: review requires non-empty head_sha"));
        }
        let created_at = required_review_string(&event, "created_at", &role, false)?;
        let updated_at = required_review_string(&event, "updated_at", &role, false)?;
        let last_edited_at = required_nullable_review_string(&event, "last_edited_at", &role)?;
        let body = required_review_string(&event, "body", &role, true)?;
        let retirement_actor_permission = if event.contains_key("retirement_actor_permission") {
            required_review_string(&event, "retirement_actor_permission", &role, true)?
        } else {
            String::new()
        };
        events.push(ReviewEvidenceEvent {
            kind,
            identity,
            author,
            state,
            head_sha,
            created_at,
            updated_at,
            last_edited_at,
            body,
            retirement_actor_permission,
        });
    }
    let head_sha = if obj.contains_key("review_snapshot_head_sha") {
        required_review_string(obj, "review_snapshot_head_sha", where_, false)?
    } else {
        default_head.to_owned()
    };
    let review_decision = match obj.get("review_snapshot_review_decision") {
        None => default_decision.to_owned(),
        Some(Value::Null) => String::new(),
        Some(Value::String(value)) => value.clone(),
        Some(_) => {
            return Err(format!(
                "{where_}: field 'review_snapshot_review_decision' must be a string or null"
            ))
        }
    };
    Ok(Some(ReviewEvidenceSnapshot {
        head_sha,
        review_decision,
        events,
    }))
}

fn fake_pr(value: &Value, where_: &str, default_base: &str) -> Result<FakePr, String> {
    let obj = object(value, where_)?;
    let number = required_integer(&obj, "number", where_, true)?;
    let head_sha = opt_string(&obj, "head_sha", &format!("sha-{number}"));
    let fetched_head_sha = opt_string(&obj, "fetched_head_sha", &head_sha);
    let api_head_sha = opt_string(&obj, "api_head_sha", &head_sha);
    let head_ref = opt_string(&obj, "head_ref", &format!("feature-{number}"));
    let base_ref = opt_string(&obj, "base_ref", default_base);
    for (field, value) in [
        ("head_ref", head_ref.as_str()),
        ("base_ref", base_ref.as_str()),
        ("head_sha", head_sha.as_str()),
        ("api_head_sha", api_head_sha.as_str()),
        ("fetched_head_sha", fetched_head_sha.as_str()),
    ] {
        if value.is_empty() {
            return Err(format!(
                "{where_}: field {field:?} must be a non-empty string"
            ));
        }
    }
    let review_decision = opt_string(&obj, "review_decision", "");
    Ok(FakePr {
        raw: RawPr {
            number,
            head_ref,
            base_ref,
            api_head_sha,
            title: opt_string(&obj, "title", ""),
            author: opt_string(&obj, "author", ""),
            is_draft: obj
                .get("is_draft")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            mergeable: opt_string(&obj, "mergeable", ""),
            review_decision: review_decision.clone(),
            review_evidence_unavailable: match obj.get("review_evidence_unavailable") {
                None => false,
                Some(Value::Bool(value)) => *value,
                Some(_) => {
                    return Err(format!(
                        "{where_}: field 'review_evidence_unavailable' must be a boolean"
                    ))
                }
            },
            created_at: opt_string(&obj, "created_at", ""),
            updated_at: opt_string(&obj, "updated_at", ""),
            additions: opt_integer(&obj, "additions", 0, where_, true)?,
            deletions: opt_integer(&obj, "deletions", 0, where_, true)?,
            labels: strings(&obj, "labels"),
            checks: checks(obj.get("checks"), where_)?,
            review_snapshot: review_snapshot(&obj, where_, &head_sha, &review_decision)?,
            mechanism_symbols: strings(&obj, "mechanism_symbols"),
        },
        fetched_head_sha,
        head_sha,
        changed_files: strings(&obj, "changed_files").into_iter().collect(),
        base_conflict_paths: strings(&obj, "base_conflict_paths"),
        commits_behind: opt_integer(&obj, "commits_behind", 0, where_, true)?,
    })
}

impl FakeHost {
    /// Build a fixture host and return it with the fixture repository and base branch.
    pub fn from_value(value: &Value) -> Result<(Self, String, String), String> {
        let doc = object(value, "<root>")?;
        let repo = opt_string(&doc, "repo", "owner/repo");
        let base = opt_string(&doc, "base", DEFAULT_BASE);
        if base.is_empty() {
            return Err("<root>: field 'base' must be a non-empty string".to_owned());
        }
        let entries = doc
            .get("prs")
            .and_then(Value::as_array)
            .ok_or("<root>: 'prs' must be a list")?;
        let prs = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| fake_pr(entry, &format!("prs[{index}]"), &base))
            .collect::<Result<Vec<_>, _>>()?;
        let mut numbers = BTreeSet::new();
        let mut head_refs = BTreeSet::new();
        let mut fixture_shas = BTreeMap::new();
        for pr in &prs {
            if !numbers.insert(pr.raw.number) {
                return Err("<root>: duplicate PR numbers are not allowed".to_owned());
            }
            if !head_refs.insert(pr.raw.head_ref.clone()) {
                return Err("<root>: duplicate PR head_ref values are ambiguous".to_owned());
            }
            for sha in [&pr.head_sha, &pr.fetched_head_sha] {
                if let Some(owner) = fixture_shas.insert(sha.clone(), pr.raw.number) {
                    if owner != pr.raw.number {
                        return Err(format!(
                            "<root>: commit identity {sha:?} is shared by PR #{owner} and PR #{}; fixture host data would be ambiguous",
                            pr.raw.number
                        ));
                    }
                }
            }
        }
        let relation_entries = |key: &str| -> Result<&[Value], String> {
            match doc.get(key) {
                None | Some(Value::Null) => Ok(&[]),
                Some(Value::Array(entries)) => Ok(entries.as_slice()),
                Some(_) => Err(format!("<root>: field {key:?} must be a list")),
            }
        };
        let mut conflicts = BTreeMap::new();
        for (index, entry) in relation_entries("conflicts")?.iter().enumerate() {
            let where_ = format!("conflicts[{index}]");
            let obj = object(entry, &where_)?;
            let a = required_integer(&obj, "a", &where_, true)?;
            let b = required_integer(&obj, "b", &where_, true)?;
            if a == b {
                return Err(format!(
                    "conflicts[{index}]: self-conflict for PR #{a} is invalid"
                ));
            }
            let unknown = [a, b]
                .into_iter()
                .filter(|number| !numbers.contains(number))
                .collect::<BTreeSet<_>>();
            if !unknown.is_empty() {
                return Err(format!(
                    "conflicts[{index}]: unknown PR endpoint(s): {}",
                    unknown
                        .iter()
                        .map(|number| format!("#{number}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let mut paths = strings(&obj, "paths");
            if paths.is_empty() {
                paths.push("<conflict>".to_owned());
            }
            let key = edge_key(a, b);
            if conflicts.insert(key, paths).is_some() {
                return Err(format!(
                    "conflicts[{index}]: duplicate conflict edge ({}, {})",
                    key.0, key.1
                ));
            }
        }
        let mut ancestry = BTreeSet::new();
        for (index, entry) in relation_entries("ancestry")?.iter().enumerate() {
            let where_ = format!("ancestry[{index}]");
            let obj = object(entry, &where_)?;
            let before = required_integer(&obj, "before", &where_, true)?;
            let after = required_integer(&obj, "after", &where_, true)?;
            if before == after {
                return Err(format!(
                    "ancestry[{index}]: self-ancestry for PR #{before} is invalid"
                ));
            }
            let unknown = [before, after]
                .into_iter()
                .filter(|number| !numbers.contains(number))
                .collect::<BTreeSet<_>>();
            if !unknown.is_empty() {
                return Err(format!(
                    "ancestry[{index}]: unknown PR endpoint(s): {}",
                    unknown
                        .iter()
                        .map(|number| format!("#{number}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !ancestry.insert((before, after)) {
                return Err(format!(
                    "ancestry[{index}]: duplicate ancestry edge ({before}, {after})"
                ));
            }
        }
        let base_shas: BTreeMap<_, _> = prs
            .iter()
            .map(|pr| {
                (
                    pr.raw.base_ref.clone(),
                    format!("basesha-{}", pr.raw.base_ref),
                )
            })
            .collect();
        let by_number = prs
            .iter()
            .enumerate()
            .map(|(index, pr)| (pr.raw.number, index))
            .collect();
        let mut number_by_sha = BTreeMap::new();
        for pr in &prs {
            number_by_sha.insert(pr.head_sha.clone(), pr.raw.number);
            number_by_sha.insert(pr.fetched_head_sha.clone(), pr.raw.number);
        }
        let base_ref_by_sha = base_shas
            .iter()
            .map(|(reference, sha)| (sha.clone(), reference.clone()))
            .collect();
        Ok((
            Self {
                prs,
                conflicts,
                ancestry,
                base_shas,
                by_number,
                number_by_sha,
                base_ref_by_sha,
            },
            repo,
            base,
        ))
    }

    fn resolve_source(&self, source: &str) -> Result<String, String> {
        if let Some(reference) = source.strip_prefix("refs/heads/") {
            return Ok(self
                .base_shas
                .get(reference)
                .cloned()
                .unwrap_or_else(|| format!("basesha-{reference}")));
        }
        if let Some(number) = source
            .strip_prefix("refs/pull/")
            .and_then(|value| value.strip_suffix("/head"))
            .and_then(|value| value.parse::<i64>().ok())
        {
            let index = self
                .by_number
                .get(&number)
                .ok_or_else(|| format!("prefetch_refs: unknown PR in {source:?}"))?;
            return Ok(self.prs[*index].fetched_head_sha.clone());
        }
        Err(format!("prefetch_refs: unrecognized source ref {source:?}"))
    }
}

impl VcsHost for FakeHost {
    fn list_open_prs(&mut self, _repo: &str, _base: Option<&str>) -> Result<Vec<RawPr>, String> {
        Ok(self.prs.iter().map(|pr| pr.raw.clone()).collect())
    }

    fn prefetch_refs(
        &mut self,
        refspecs: &[(String, String)],
    ) -> Result<BTreeMap<String, String>, String> {
        refspecs
            .iter()
            .map(|(source, dest)| Ok((dest.clone(), self.resolve_source(source)?)))
            .collect()
    }

    fn merge_tree(&mut self, left: &str, right: &str) -> Result<Vec<String>, String> {
        let left_pr = self.number_by_sha.get(left).copied();
        let right_pr = self.number_by_sha.get(right).copied();
        if self.base_ref_by_sha.contains_key(left) {
            if let Some(number) = right_pr {
                return Ok(self.prs[self.by_number[&number]]
                    .base_conflict_paths
                    .clone());
            }
        }
        if self.base_ref_by_sha.contains_key(right) {
            if let Some(number) = left_pr {
                return Ok(self.prs[self.by_number[&number]]
                    .base_conflict_paths
                    .clone());
            }
        }
        Ok(left_pr
            .zip(right_pr)
            .and_then(|(a, b)| self.conflicts.get(&edge_key(a, b)).cloned())
            .unwrap_or_default())
    }

    fn is_ancestor(&mut self, ancestor: &str, descendant: &str) -> Result<bool, String> {
        Ok(self
            .number_by_sha
            .get(ancestor)
            .zip(self.number_by_sha.get(descendant))
            .is_some_and(|(a, b)| self.ancestry.contains(&(*a, *b))))
    }

    fn changed_files(
        &mut self,
        _base_sha: &str,
        head_sha: &str,
    ) -> Result<BTreeSet<String>, String> {
        Ok(self
            .number_by_sha
            .get(head_sha)
            .map(|number| self.prs[self.by_number[number]].changed_files.clone())
            .unwrap_or_default())
    }

    fn commits_behind(&mut self, head_sha: &str, _base_sha: &str) -> Result<i64, String> {
        Ok(self
            .number_by_sha
            .get(head_sha)
            .map(|number| self.prs[self.by_number[number]].commits_behind)
            .unwrap_or(0))
    }
}

/// Parse strict JSON or YAML, rejecting duplicate and non-string mapping keys.
pub fn load_fixture_text(text: &str, as_yaml: bool) -> Result<Value, String> {
    if as_yaml {
        serde_norway::from_str::<UniqueValue>(text)
            .map(|value| value.0)
            .map_err(|error| format!("invalid YAML fixture: {error}"))
    } else {
        serde_json::from_str::<UniqueValue>(text)
            .map(|value| value.0)
            .map_err(|error| format!("invalid JSON fixture: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_yaml_defaults_and_simulates_drift() {
        let raw =
            load_fixture_text("prs:\n  - number: 7\n    fetched_head_sha: moved\n", true).unwrap();
        let (mut host, repo, base) = FakeHost::from_value(&raw).unwrap();
        assert_eq!((repo.as_str(), base.as_str()), ("owner/repo", "main"));
        let prs = host.list_open_prs("", None).unwrap();
        assert_eq!(prs[0].api_head_sha, "sha-7");
        let fetched = host
            .prefetch_refs(&[("refs/pull/7/head".into(), "x".into())])
            .unwrap();
        assert_eq!(fetched["x"], "moved");
    }

    #[test]
    fn review_evidence_unavailable_is_a_strict_boolean() {
        for malformed in [Value::Null, json!(0), json!("true"), json!([]), json!({})] {
            let fixture = json!({
                "prs": [{
                    "number": 7,
                    "review_evidence_unavailable": malformed
                }]
            });
            let error = FakeHost::from_value(&fixture)
                .err()
                .expect("malformed boolean must be rejected");
            assert!(error.contains("field 'review_evidence_unavailable' must be a boolean"));
        }
    }
}
