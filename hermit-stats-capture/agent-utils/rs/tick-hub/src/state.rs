//! Strict per-host runtime state and state-machine output.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;

use indexmap::IndexMap;
use serde_json::Value;

use crate::emit::{format_action, format_note};
use crate::io::parse_yaml_value;
use crate::text::{string_repr, trim};

/// Desired tick cadence used by the default state.
pub const DEFAULT_TICK_FREQUENCY_MIN: i64 = 30;

/// Scalar values accepted in the caller-owned flag map.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FlagValue {
    /// Boolean flag.
    Bool(bool),
    /// Signed integer flag.
    Int(i64),
    /// String flag.
    String(String),
}

impl FlagValue {
    /// Whether this value enables a required flag.
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Bool(value) => *value,
            Self::Int(value) => *value != 0,
            Self::String(value) => !value.is_empty(),
        }
    }
}

impl From<bool> for FlagValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for FlagValue {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<String> for FlagValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for FlagValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

/// Strict ops-state validation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateError(pub String);

impl fmt::Display for StateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for StateError {}

/// Per-host runtime state for one tick.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpsState {
    /// Master switch. Disabled state still emits health and state lines.
    pub enabled: bool,
    /// Desired scheduler cadence in minutes.
    pub tick_frequency_min: i64,
    /// Optional host identity shown in the state summary.
    pub label: Option<String>,
    /// Caller-owned typed feature flags.
    pub flags: BTreeMap<String, FlagValue>,
}

impl Default for OpsState {
    fn default() -> Self {
        Self {
            enabled: true,
            tick_frequency_min: DEFAULT_TICK_FREQUENCY_MIN,
            label: None,
            flags: BTreeMap::new(),
        }
    }
}

impl OpsState {
    /// Construct the enabled fallback state.
    pub fn default_enabled() -> Self {
        Self::default()
    }

    /// Parse strict YAML using YAML-1.2 core scalar rules.
    pub fn from_yaml(text: &str) -> Result<Self, StateError> {
        let raw = parse_yaml_value(text).map_err(StateError)?;
        Self::from_value(&raw)
    }

    /// Load and parse an ops-state file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let text =
            fs::read_to_string(path.as_ref()).map_err(|error| StateError(error.to_string()))?;
        Self::from_yaml(&text)
    }

    /// Validate a deserialized state mapping.
    pub fn from_value(raw: &Value) -> Result<Self, StateError> {
        let Some(obj) = raw.as_object() else {
            return Err(StateError(
                "ops-state file must contain a top-level mapping".to_string(),
            ));
        };
        let allowed = ["enabled", "tick_frequency_min", "label", "flags"];
        let unknown: BTreeSet<_> = obj
            .keys()
            .filter(|key| !allowed.contains(&key.as_str()) && !key.starts_with('_'))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err(StateError(format!(
                "top-level: unknown key(s) {:?} (allowed: {:?})",
                unknown.into_iter().collect::<Vec<_>>(),
                allowed
            )));
        }
        let enabled = obj.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
            let value = obj.get("enabled").unwrap_or(&Value::Null);
            StateError(format!(
                "enabled must be a boolean (got {}: {})",
                value_type(value),
                py_repr(value)
            ))
        })?;
        let tick = obj
            .get("tick_frequency_min")
            .map_or(Some(DEFAULT_TICK_FREQUENCY_MIN), Value::as_i64)
            .ok_or_else(|| {
                let tick_value = obj
                    .get("tick_frequency_min")
                    .expect("a missing value uses the default above");
                StateError(format!(
                    "tick_frequency_min must be an integer (got {})",
                    py_repr(tick_value)
                ))
            })?;
        if tick <= 0 {
            return Err(StateError(format!(
                "tick_frequency_min must be positive (got {tick})"
            )));
        }
        let label = match obj.get("label") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) => {
                let trimmed = trim(value);
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            }
            Some(value) => {
                return Err(StateError(format!(
                    "label must be a string or null (got {}: {})",
                    value_type(value),
                    py_repr(value)
                )))
            }
        };
        let flags = parse_flags(obj.get("flags"))?;
        Ok(Self {
            enabled,
            tick_frequency_min: tick,
            label,
            flags,
        })
    }

    /// Serialize to stable YAML with flags sorted by key.
    pub fn to_yaml(&self) -> Result<String, StateError> {
        let canonical = Self::from_value(&self.to_value())?;
        serde_norway::to_string(&canonical.to_value())
            .map_err(|error| StateError(format!("cannot serialize YAML: {error}")))
    }

    fn to_value(&self) -> Value {
        let mut root = serde_json::Map::new();
        root.insert("enabled".into(), Value::Bool(self.enabled));
        root.insert(
            "tick_frequency_min".into(),
            Value::Number(self.tick_frequency_min.into()),
        );
        root.insert(
            "label".into(),
            self.label.clone().map_or(Value::Null, Value::String),
        );
        let mut flags = serde_json::Map::new();
        for (key, value) in &self.flags {
            let raw = match value {
                FlagValue::Bool(value) => Value::Bool(*value),
                FlagValue::Int(value) => Value::Number((*value).into()),
                FlagValue::String(value) => Value::String(value.clone()),
            };
            flags.insert(key.clone(), raw);
        }
        root.insert("flags".into(), Value::Object(flags));
        Value::Object(root)
    }
}

fn parse_flags(value: Option<&Value>) -> Result<BTreeMap<String, FlagValue>, StateError> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let Some(obj) = value.as_object() else {
        return Err(StateError(format!(
            "flags must be a mapping (got {})",
            value_type(value)
        )));
    };
    let mut out = BTreeMap::new();
    for (name, raw) in obj {
        if name == "<<" {
            return Err(StateError(
                "reserved mapping key '<<' is not allowed".to_string(),
            ));
        }
        let value = match raw {
            Value::Bool(value) => FlagValue::Bool(*value),
            Value::Number(value) => value.as_i64().map(FlagValue::Int).ok_or_else(|| {
                StateError(format!(
                    "flags.{name} must be a boolean, integer, or string (got int: {value})"
                ))
            })?,
            Value::String(value) => FlagValue::String(value.clone()),
            other => {
                return Err(StateError(format!(
                    "flags.{name} must be a boolean, integer, or string (got {}: {})",
                    value_type(other),
                    py_repr(other)
                )))
            }
        };
        out.insert(name.clone(), value);
    }
    Ok(out)
}

/// Return whether a named state flag exists and is truthy.
pub fn flag_truthy(flags: &BTreeMap<String, FlagValue>, name: &str) -> bool {
    flags.get(name).is_some_and(FlagValue::is_truthy)
}

fn flags_summary(flags: &BTreeMap<String, FlagValue>) -> String {
    if flags.is_empty() {
        return String::new();
    }
    let rendered = flags
        .iter()
        .map(|(key, value)| {
            let value = match value {
                FlagValue::Bool(true) => "true".to_string(),
                FlagValue::Bool(false) => "false".to_string(),
                FlagValue::Int(value) => value.to_string(),
                FlagValue::String(value) => value.clone(),
            };
            format!("{key}={value}")
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(" flags={rendered}")
}

/// Render the ops-state summary, cadence actualization, and disabled note.
pub fn state_lines(state: &OpsState, current_tick_min: Option<i64>) -> Vec<String> {
    let label = state
        .label
        .as_ref()
        .map_or_else(String::new, |label| format!(" label={label}"));
    let mut lines = vec![format_note(&format!(
        "ops-state enabled={} tick_frequency_min={}{}{}",
        if state.enabled { "true" } else { "false" },
        state.tick_frequency_min,
        label,
        flags_summary(&state.flags)
    ))];
    if current_tick_min.is_some_and(|current| current != state.tick_frequency_min) {
        let current = current_tick_min.unwrap();
        let fields = IndexMap::from([
            ("desired".into(), state.tick_frequency_min.to_string()),
            ("current".into(), current.to_string()),
        ]);
        lines.push(format_action(
            "actualize-tick-frequency",
            &fields,
            &format!(
                "reschedule the tick to {} min so it matches the intended cadence (currently {} min)",
                state.tick_frequency_min, current
            ),
        ));
    }
    if !state.enabled {
        lines.push(format_note(
            "ops-state disabled — no reminders will fire this tick; set enabled: true to activate",
        ));
    }
    lines
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_i64() || number.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(text) => string_repr(text),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_norway_safe_flags_and_trims_label() {
        let state = OpsState::from_yaml(
            "enabled: true\ntick_frequency_min: 45\nlabel: ' my-host '\nflags:\n  benchmark_enabled: true\n  region: no\n  count: 3\n",
        )
        .unwrap();
        assert_eq!(state.label.as_deref(), Some("my-host"));
        assert_eq!(
            state.flags.get("region"),
            Some(&FlagValue::String("no".into()))
        );
        assert!(flag_truthy(&state.flags, "count"));
    }

    #[test]
    fn flag_integers_accept_only_signed_i64_range() {
        let state = OpsState::from_yaml(
            "enabled: true\nflags:\n  low: -9223372036854775808\n  high: 9223372036854775807\n",
        )
        .unwrap();
        assert_eq!(state.flags.get("low"), Some(&FlagValue::Int(i64::MIN)));
        assert_eq!(state.flags.get("high"), Some(&FlagValue::Int(i64::MAX)));
        assert!(
            OpsState::from_yaml("enabled: true\nflags:\n  too_big: 9223372036854775808\n").is_err()
        );
        assert!(
            OpsState::from_yaml("enabled: true\nflags:\n  too_small: -9223372036854775809\n")
                .is_err()
        );
    }

    #[test]
    fn strict_validation_rejects_unknown_and_wrong_types() {
        assert!(OpsState::from_yaml("tick_frequency_min: 30\n").is_err());
        assert!(OpsState::from_yaml("enabled: 1\n").is_err());
        assert!(OpsState::from_yaml("enabled: true\nunknown_key: 1\n").is_err());
        assert!(OpsState::from_yaml("enabled: true\nflags: [1]\n").is_err());
        assert!(OpsState::from_yaml("enabled: true\nflags: null\n").is_err());
        assert!(
            OpsState::from_yaml("enabled: true\ntick_frequency_min: 9223372036854775808\n")
                .is_err()
        );
        assert!(
            OpsState::from_yaml("enabled: true\ntick_frequency_min: -9223372036854775809\n")
                .is_err()
        );
        assert!(OpsState::from_yaml("enabled: true\nflags:\n  1: value\n").is_err());
        assert!(OpsState::from_yaml("enabled: true\nflags:\n  x: 1\n  x: 2\n").is_err());
        assert!(OpsState::from_yaml("enabled: true\n_note:\n  <<: value\n").is_err());
    }

    #[test]
    fn state_lines_are_stable() {
        let state = OpsState {
            enabled: true,
            tick_frequency_min: 30,
            label: Some("h".into()),
            flags: BTreeMap::from([("x".into(), FlagValue::Bool(true))]),
        };
        let lines = state_lines(&state, Some(15));
        assert_eq!(
            lines[0],
            "NOTE: ops-state enabled=true tick_frequency_min=30 label=h flags=x=true"
        );
        assert!(lines[1].starts_with("ACTION: actualize-tick-frequency desired=30 current=15"));
    }

    #[test]
    fn state_yaml_round_trips_sorted_typed_flags() {
        let state = OpsState {
            enabled: false,
            tick_frequency_min: 10,
            label: Some("host".into()),
            flags: BTreeMap::from([
                ("numeric-looking".into(), FlagValue::String("-0xF0".into())),
                ("retries".into(), FlagValue::Int(3)),
                ("switch".into(), FlagValue::Bool(true)),
            ]),
        };
        let yaml = state.to_yaml().unwrap();
        assert_eq!(OpsState::from_yaml(&yaml).unwrap(), state);
        assert!(yaml.contains("'-0xF0'") || yaml.contains("\"-0xF0\""));
    }

    #[test]
    fn invalid_direct_state_is_rejected_before_serialization() {
        let invalid_tick = OpsState {
            tick_frequency_min: 0,
            ..OpsState::default()
        };
        assert!(invalid_tick.to_yaml().is_err());

        let canonical = OpsState {
            label: Some(" host ".into()),
            ..OpsState::default()
        }
        .to_yaml()
        .unwrap();
        assert_eq!(
            OpsState::from_yaml(&canonical).unwrap().label.as_deref(),
            Some("host")
        );
    }

    #[test]
    fn reserved_merge_key_is_rejected_in_state_maps() {
        assert!(OpsState::from_yaml("enabled: true\nflags: {'<<': value}\n").is_err());
        let state = OpsState {
            flags: BTreeMap::from([("<<".into(), FlagValue::Bool(true))]),
            ..OpsState::default()
        };
        assert!(state.to_yaml().is_err());
    }
}
