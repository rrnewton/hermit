//! Strict JSON/YAML loading and canonical serialization for [`TickConfig`].

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use indexmap::IndexMap;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::cadence::INTERNAL_STATE_PREFIX;
use crate::model::{Emit, EmitKind, Gate, GateWhen, HealthCheck, Reminder, TickConfig};
use crate::text::{is_whitespace, string_repr, trim};

/// Configuration parse or serialization failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TickConfigError(pub String);

impl fmt::Display for TickConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for TickConfigError {}

/// Format-neutral value tree that retains duplicate-key information while Serde is visiting a
/// document. `YAML` enables YAML-only key rules such as rejecting merge keys.
enum StrictValue<const YAML: bool> {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    String(String),
    Sequence(Vec<Self>),
    Mapping(IndexMap<String, Self>),
}

impl<'de, const YAML: bool> Deserialize<'de> for StrictValue<YAML> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor::<YAML>)
    }
}

struct StrictValueVisitor<const YAML: bool>;

impl<'de, const YAML: bool> Visitor<'de> for StrictValueVisitor<YAML> {
    type Value = StrictValue<YAML>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON or YAML value")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictValue::I64(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictValue::U64(value))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.is_finite() {
            Ok(StrictValue::F64(value))
        } else {
            Err(E::custom("non-finite numbers are not allowed"))
        }
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(StrictValue::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictValue::String(value))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element()? {
            values.push(value);
        }
        Ok(StrictValue::Sequence(values))
    }

    fn visit_map<A>(self, mut mapping: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = IndexMap::new();
        while let Some(raw_key) = mapping.next_key::<StrictValue<YAML>>()? {
            let StrictValue::String(key) = raw_key else {
                return Err(<A::Error as de::Error>::custom(
                    "mapping keys must be strings",
                ));
            };
            if key == "<<" {
                return Err(<A::Error as de::Error>::custom(
                    "reserved mapping key '<<' is not allowed",
                ));
            }
            if values.contains_key(&key) {
                let kind = if YAML { "mapping" } else { "object" };
                return Err(<A::Error as de::Error>::custom(format!(
                    "duplicate {kind} key {}",
                    string_repr(&key)
                )));
            }
            let value = mapping.next_value()?;
            values.insert(key, value);
        }
        Ok(StrictValue::Mapping(values))
    }
}

fn strict_value_to_json<const YAML: bool>(value: StrictValue<YAML>) -> Result<Value, String> {
    match value {
        StrictValue::Null => Ok(Value::Null),
        StrictValue::Bool(value) => Ok(Value::Bool(value)),
        StrictValue::I64(value) => Ok(Value::Number(value.into())),
        StrictValue::U64(value) => Ok(Value::Number(value.into())),
        StrictValue::F64(value) => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| "non-finite numbers are not allowed".to_string()),
        StrictValue::String(value) => Ok(Value::String(value)),
        StrictValue::Sequence(values) => values
            .into_iter()
            .map(strict_value_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        StrictValue::Mapping(values) => {
            let mut object = Map::new();
            for (key, value) in values {
                object.insert(key, strict_value_to_json(value)?);
            }
            Ok(Value::Object(object))
        }
    }
}

fn as_obj<'a>(value: &'a Value, where_: &str) -> Result<&'a Map<String, Value>, TickConfigError> {
    value.as_object().ok_or_else(|| {
        TickConfigError(format!(
            "{where_}: expected an object, got {}",
            value_type(value)
        ))
    })
}

fn reject_unknown(
    object: &Map<String, Value>,
    allowed: &[&str],
    where_: &str,
) -> Result<(), TickConfigError> {
    let unknown = object
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(TickConfigError(format!(
            "{where_}: unknown field(s): {}",
            unknown.into_iter().collect::<Vec<_>>().join(", ")
        )))
    }
}

fn req_str(
    object: &Map<String, Value>,
    key: &str,
    where_: &str,
) -> Result<String, TickConfigError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| TickConfigError(format!("{where_}: field '{key}' must be a string")))
}

fn opt_str(
    object: &Map<String, Value>,
    key: &str,
    default: &str,
    where_: &str,
) -> Result<String, TickConfigError> {
    match object.get(key) {
        None => Ok(default.to_string()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(TickConfigError(format!(
            "{where_}: field '{key}' must be a string"
        ))),
    }
}

fn opt_int(
    object: &Map<String, Value>,
    key: &str,
    default: i64,
    where_: &str,
) -> Result<i64, TickConfigError> {
    match object.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_i64()
            .ok_or_else(|| TickConfigError(format!("{where_}: field '{key}' must be an integer"))),
    }
}

fn opt_nonnegative_int(
    object: &Map<String, Value>,
    key: &str,
    default: i64,
    where_: &str,
) -> Result<i64, TickConfigError> {
    let value = opt_int(object, key, default, where_)?;
    if value < 0 {
        return Err(TickConfigError(format!(
            "{where_}: field '{key}' must be non-negative"
        )));
    }
    Ok(value)
}

fn opt_bool(
    object: &Map<String, Value>,
    key: &str,
    default: bool,
    where_: &str,
) -> Result<bool, TickConfigError> {
    match object.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| TickConfigError(format!("{where_}: field '{key}' must be a boolean"))),
    }
}

fn opt_str_list(
    object: &Map<String, Value>,
    key: &str,
    where_: &str,
) -> Result<Vec<String>, TickConfigError> {
    let Some(value) = object.get(key) else {
        return Ok(Vec::new());
    };
    let array = value.as_array().ok_or_else(|| {
        TickConfigError(format!("{where_}: field '{key}' must be a list of strings"))
    })?;
    let mut out = Vec::with_capacity(array.len());
    for item in array {
        let Some(item) = item.as_str() else {
            return Err(TickConfigError(format!(
                "{where_}: field '{key}' must contain only strings"
            )));
        };
        out.push(item.to_string());
    }
    Ok(out)
}

fn opt_fields(
    object: &Map<String, Value>,
    key: &str,
    where_: &str,
) -> Result<IndexMap<String, String>, TickConfigError> {
    let Some(value) = object.get(key) else {
        return Ok(IndexMap::new());
    };
    let fields = as_obj(value, &format!("{where_}.{key}"))?;
    let mut out = IndexMap::new();
    for (name, value) in fields {
        let Some(value) = value.as_str() else {
            return Err(TickConfigError(format!(
                "{where_}.{key}.{name}: must be a string"
            )));
        };
        out.insert(name.clone(), value.to_string());
    }
    Ok(out)
}

fn gate_from(value: Option<&Value>, where_: &str) -> Result<Option<Gate>, TickConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let object = as_obj(value, where_)?;
    reject_unknown(
        object,
        &["cmd", "when", "capture", "timeout_secs", "parallel"],
        where_,
    )?;
    let when_name = opt_str(object, "when", "success", where_)?;
    let when = GateWhen::from_value(&when_name).ok_or_else(|| {
        TickConfigError(format!(
            "{where_}.when: unknown value {} (allowed: ['success', 'failure', 'nonempty', 'always'])",
            string_repr(&when_name)
        ))
    })?;
    // A non-positive bound would terminate the command before it could produce anything,
    // which reads as a gate that always times out. Refuse it rather than accept a value that
    // can only ever yield NO-SIGNAL.
    let timeout_secs = match object.get("timeout_secs") {
        Some(value) if !value.is_null() => {
            let secs = opt_nonnegative_int(object, "timeout_secs", 0, where_)?;
            if secs <= 0 {
                return Err(TickConfigError(format!(
                    "{where_}: 'timeout_secs' ({secs}) must be greater than 0"
                )));
            }
            Some(secs)
        }
        _ => None,
    };
    Ok(Some(Gate {
        cmd: req_str(object, "cmd", where_)?,
        when,
        capture: opt_bool(object, "capture", false, where_)?,
        parallel: opt_bool(object, "parallel", false, where_)?,
        timeout_secs,
    }))
}

fn emit_from(value: &Value, where_: &str) -> Result<Emit, TickConfigError> {
    let object = as_obj(value, where_)?;
    reject_unknown(object, &["kind", "title", "skill", "fields"], where_)?;
    let kind_name = opt_str(object, "kind", "action", where_)?;
    let kind = EmitKind::from_value(&kind_name).ok_or_else(|| {
        TickConfigError(format!(
            "{where_}.kind: unknown value {} (allowed: ['action', 'note'])",
            string_repr(&kind_name)
        ))
    })?;
    let emit = Emit {
        kind,
        title: opt_str(object, "title", "", where_)?,
        skill: opt_str(object, "skill", "", where_)?,
        fields: opt_fields(object, "fields", where_)?,
    };
    if emit.kind == EmitKind::Action && trim(&emit.skill).is_empty() {
        return Err(TickConfigError(format!(
            "{where_}: an ACTION emit requires a non-empty 'skill'"
        )));
    }
    Ok(emit)
}

fn reminder_from(value: &Value, where_: &str) -> Result<Reminder, TickConfigError> {
    let object = as_obj(value, where_)?;
    reject_unknown(
        object,
        &[
            "name",
            "emit",
            "cadence_secs",
            "cadence_offset_secs",
            "cadence_window_secs",
            "requires_flags",
            "depends_on",
            "gate",
        ],
        where_,
    )?;
    let emit = object
        .get("emit")
        .filter(|value| !value.is_null())
        .ok_or_else(|| TickConfigError(format!("{where_}: field 'emit' is required")))?;
    let name = req_str(object, "name", where_)?;
    if trim(&name).is_empty() {
        return Err(TickConfigError(format!(
            "{where_}: field 'name' must be non-empty"
        )));
    }
    if name.contains('=') || name.chars().any(is_whitespace) {
        return Err(TickConfigError(format!(
            "{where_}: field 'name' must not contain whitespace or '=' (it is a fired-state key)"
        )));
    }
    if name.starts_with(INTERNAL_STATE_PREFIX) {
        return Err(TickConfigError(format!(
            "{where_}: field 'name' must not start with reserved internal-state prefix {INTERNAL_STATE_PREFIX:?}"
        )));
    }
    let cadence_secs = opt_nonnegative_int(object, "cadence_secs", 0, where_)?;
    // Refuse a phase that cannot mean what it says rather than silently normalising it, and
    // keep every message identical to the Python edition's so the two stay one contract.
    let cadence_offset_secs = match object.get("cadence_offset_secs") {
        Some(value) if !value.is_null() => {
            let offset = opt_nonnegative_int(object, "cadence_offset_secs", 0, where_)?;
            if cadence_secs <= 0 {
                return Err(TickConfigError(format!(
                    "{where_}: 'cadence_offset_secs' requires a positive 'cadence_secs'; \
                     an every-tick reminder is always due and cannot be phased"
                )));
            }
            if offset >= cadence_secs {
                return Err(TickConfigError(format!(
                    "{where_}: 'cadence_offset_secs' ({offset}) must be less than \
                     'cadence_secs' ({cadence_secs}); a larger offset names the same instants \
                     as offset % cadence and hides which tick the author meant"
                )));
            }
            Some(offset)
        }
        _ => None,
    };
    let cadence_window_secs = match object.get("cadence_window_secs") {
        Some(value) if !value.is_null() => {
            let window = opt_nonnegative_int(object, "cadence_window_secs", 0, where_)?;
            if cadence_offset_secs.is_none() {
                return Err(TickConfigError(format!(
                    "{where_}: 'cadence_window_secs' requires 'cadence_offset_secs'; \
                     a window only means anything relative to a phase"
                )));
            }
            if window <= 0 || window > cadence_secs {
                return Err(TickConfigError(format!(
                    "{where_}: 'cadence_window_secs' ({window}) must be greater than 0 and at \
                     most 'cadence_secs' ({cadence_secs})"
                )));
            }
            Some(window)
        }
        _ => None,
    };
    Ok(Reminder {
        name,
        emit: emit_from(emit, &format!("{where_}.emit"))?,
        cadence_secs,
        requires_flags: opt_str_list(object, "requires_flags", where_)?,
        gate: gate_from(object.get("gate"), &format!("{where_}.gate"))?,
        depends_on: opt_str_list(object, "depends_on", where_)?,
        cadence_offset_secs,
        cadence_window_secs,
    })
}

fn validate_dependencies(reminders: &[Reminder]) -> Result<(), TickConfigError> {
    let by_name: BTreeMap<&str, &Reminder> = reminders
        .iter()
        .map(|reminder| (reminder.name.as_str(), reminder))
        .collect();
    for reminder in reminders {
        let mut unique = BTreeSet::new();
        for dependency in &reminder.depends_on {
            if trim(dependency).is_empty()
                || dependency.contains('=')
                || dependency.chars().any(is_whitespace)
            {
                return Err(TickConfigError(format!(
                    "<root>: reminder {:?} has invalid depends_on name {:?}",
                    reminder.name, dependency
                )));
            }
            if !unique.insert(dependency.as_str()) {
                return Err(TickConfigError(format!(
                    "<root>: reminder {:?} has duplicate depends_on entries",
                    reminder.name
                )));
            }
            if dependency == &reminder.name {
                return Err(TickConfigError(format!(
                    "<root>: reminder {:?} cannot depend on itself",
                    reminder.name
                )));
            }
            if !by_name.contains_key(dependency.as_str()) {
                return Err(TickConfigError(format!(
                    "<root>: reminder {:?} depends on unknown reminder {:?}",
                    reminder.name, dependency
                )));
            }
        }
    }

    fn visit<'a>(
        name: &'a str,
        by_name: &BTreeMap<&'a str, &'a Reminder>,
        visiting: &mut BTreeSet<&'a str>,
        visited: &mut BTreeSet<&'a str>,
        path: &mut Vec<&'a str>,
    ) -> Result<(), TickConfigError> {
        if visited.contains(name) {
            return Ok(());
        }
        if !visiting.insert(name) {
            path.push(name);
            return Err(TickConfigError(format!(
                "<root>: reminder dependency cycle: {}",
                path.join(" -> ")
            )));
        }
        path.push(name);
        for dependency in &by_name[name].depends_on {
            visit(dependency, by_name, visiting, visited, path)?;
        }
        path.pop();
        visiting.remove(name);
        visited.insert(name);
        Ok(())
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for reminder in reminders {
        visit(
            &reminder.name,
            &by_name,
            &mut visiting,
            &mut visited,
            &mut Vec::new(),
        )?;
    }
    Ok(())
}

fn health_from(value: &Value, where_: &str) -> Result<HealthCheck, TickConfigError> {
    let object = as_obj(value, where_)?;
    reject_unknown(
        object,
        &["name", "glob", "threshold_secs", "detail"],
        where_,
    )?;
    let name = req_str(object, "name", where_)?;
    if trim(&name).is_empty() {
        return Err(TickConfigError(format!(
            "{where_}: field 'name' must be non-empty"
        )));
    }
    let glob = req_str(object, "glob", where_)?;
    if trim(&glob).is_empty() {
        return Err(TickConfigError(format!(
            "{where_}: field 'glob' must be non-empty"
        )));
    }
    Ok(HealthCheck {
        name,
        glob,
        threshold_secs: opt_nonnegative_int(object, "threshold_secs", 0, where_)?,
        detail: opt_str(object, "detail", "", where_)?,
    })
}

fn config_from_value(raw: &Value) -> Result<TickConfig, TickConfigError> {
    let object = as_obj(raw, "<root>")?;
    reject_unknown(
        object,
        &["reminders", "health_checks", "description"],
        "<root>",
    )?;
    let reminders = match object.get("reminders") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(index, value)| reminder_from(value, &format!("reminders[{index}]")))
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(TickConfigError(
                "<root>: 'reminders' must be a list".to_string(),
            ))
        }
    };
    let mut reminder_names = BTreeSet::new();
    for reminder in &reminders {
        if !reminder_names.insert(reminder.name.as_str()) {
            return Err(TickConfigError(
                "<root>: reminder names must be unique".to_string(),
            ));
        }
    }
    validate_dependencies(&reminders)?;
    let health_checks = match object.get("health_checks") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(index, value)| health_from(value, &format!("health_checks[{index}]")))
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(TickConfigError(
                "<root>: 'health_checks' must be a list".to_string(),
            ))
        }
    };
    let mut health_names = BTreeSet::new();
    for check in &health_checks {
        if !health_names.insert(check.name.as_str()) {
            return Err(TickConfigError(
                "<root>: health check names must be unique".to_string(),
            ));
        }
    }
    Ok(TickConfig {
        reminders,
        health_checks,
        description: opt_str(object, "description", "", "<root>")?,
    })
}

/// Parse strict JSON into a tick configuration.
pub fn config_from_json(text: &str) -> Result<TickConfig, TickConfigError> {
    let strict: StrictValue<false> = serde_json::from_str(text)
        .map_err(|error| TickConfigError(format!("invalid JSON: {error}")))?;
    let raw = strict_value_to_json(strict).map_err(TickConfigError)?;
    config_from_value(&raw)
}

/// Parse YAML-1.2-core configuration into the same strict model as JSON.
pub fn config_from_yaml(text: &str) -> Result<TickConfig, TickConfigError> {
    let raw = parse_yaml_value(text).map_err(TickConfigError)?;
    config_from_value(&raw)
}

/// Parse through YAML's native value tree so non-string mapping keys can be rejected before the
/// data enters JSON's string-key-only representation.
pub(crate) fn parse_yaml_value(text: &str) -> Result<Value, String> {
    let strict: StrictValue<true> =
        serde_norway::from_str(text).map_err(|error| format!("invalid YAML: {error}"))?;
    strict_value_to_json(strict).map_err(|error| format!("invalid YAML: {error}"))
}

fn gate_to_value(gate: Option<&Gate>) -> Value {
    let Some(gate) = gate else {
        return Value::Null;
    };
    let mut object = Map::new();
    object.insert("cmd".into(), Value::String(gate.cmd.clone()));
    object.insert("when".into(), Value::String(gate.when.as_str().into()));
    object.insert("capture".into(), Value::Bool(gate.capture));
    // Key order matches the Python edition exactly: cmd, when, capture, timeout_secs,
    // parallel. The two editions are one contract and their `json` output is compared
    // byte-for-byte, so a reordered key is a real divergence even though the content agrees.
    if let Some(secs) = gate.timeout_secs {
        object.insert("timeout_secs".into(), Value::Number(secs.into()));
    }
    if gate.parallel {
        object.insert("parallel".into(), Value::Bool(true));
    }
    Value::Object(object)
}

fn emit_to_value(emit: &Emit) -> Value {
    let mut object = Map::new();
    object.insert("kind".into(), Value::String(emit.kind.as_str().into()));
    object.insert("title".into(), Value::String(emit.title.clone()));
    object.insert("skill".into(), Value::String(emit.skill.clone()));
    let mut field_names: Vec<_> = emit.fields.keys().collect();
    field_names.sort();
    let mut fields = Map::new();
    for name in field_names {
        fields.insert(name.clone(), Value::String(emit.fields[name].clone()));
    }
    object.insert("fields".into(), Value::Object(fields));
    Value::Object(object)
}

fn config_to_value(config: &TickConfig) -> Value {
    let mut root = Map::new();
    root.insert(
        "description".into(),
        Value::String(config.description.clone()),
    );
    let health = config
        .health_checks
        .iter()
        .map(|check| {
            let mut object = Map::new();
            object.insert("name".into(), Value::String(check.name.clone()));
            object.insert("glob".into(), Value::String(check.glob.clone()));
            object.insert(
                "threshold_secs".into(),
                Value::Number(check.threshold_secs.into()),
            );
            object.insert("detail".into(), Value::String(check.detail.clone()));
            Value::Object(object)
        })
        .collect();
    root.insert("health_checks".into(), Value::Array(health));
    let reminders = config
        .reminders
        .iter()
        .map(|reminder| {
            let mut object = Map::new();
            object.insert("name".into(), Value::String(reminder.name.clone()));
            object.insert(
                "cadence_secs".into(),
                Value::Number(reminder.cadence_secs.into()),
            );
            if let Some(offset) = reminder.cadence_offset_secs {
                object.insert("cadence_offset_secs".into(), Value::Number(offset.into()));
            }
            if let Some(window) = reminder.cadence_window_secs {
                object.insert("cadence_window_secs".into(), Value::Number(window.into()));
            }
            object.insert(
                "requires_flags".into(),
                Value::Array(
                    reminder
                        .requires_flags
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
            if !reminder.depends_on.is_empty() {
                object.insert(
                    "depends_on".into(),
                    Value::Array(
                        reminder
                            .depends_on
                            .iter()
                            .cloned()
                            .map(Value::String)
                            .collect(),
                    ),
                );
            }
            object.insert("gate".into(), gate_to_value(reminder.gate.as_ref()));
            object.insert("emit".into(), emit_to_value(&reminder.emit));
            Value::Object(object)
        })
        .collect();
    root.insert("reminders".into(), Value::Array(reminders));
    Value::Object(root)
}

fn validate_serializable(config: &TickConfig) -> Result<(), TickConfigError> {
    let mut reminder_names = BTreeSet::new();
    for reminder in &config.reminders {
        if trim(&reminder.name).is_empty()
            || reminder.name.contains('=')
            || reminder.name.chars().any(is_whitespace)
            || reminder.name.starts_with(INTERNAL_STATE_PREFIX)
        {
            return Err(TickConfigError(
                "reminder names must be valid fired-state keys (nonempty, without whitespace or '=', and outside the reserved internal-state namespace)"
                    .to_string(),
            ));
        }
        if !reminder_names.insert(reminder.name.as_str()) {
            return Err(TickConfigError(
                "<root>: reminder names must be unique".to_string(),
            ));
        }
        if reminder.cadence_secs < 0 {
            return Err(TickConfigError(
                "reminder cadence_secs must be non-negative".to_string(),
            ));
        }
        if reminder.emit.kind == EmitKind::Action && trim(&reminder.emit.skill).is_empty() {
            return Err(TickConfigError(
                "an ACTION emit requires a non-empty skill".to_string(),
            ));
        }
        if reminder.emit.fields.contains_key("<<") {
            return Err(TickConfigError(
                "reserved mapping key '<<' is not allowed".to_string(),
            ));
        }
    }
    validate_dependencies(&config.reminders)?;
    let mut health_names = BTreeSet::new();
    for check in &config.health_checks {
        if trim(&check.name).is_empty() {
            return Err(TickConfigError(
                "health-check names must be non-empty".to_string(),
            ));
        }
        if !health_names.insert(check.name.as_str()) {
            return Err(TickConfigError(
                "<root>: health check names must be unique".to_string(),
            ));
        }
        if trim(&check.glob).is_empty() {
            return Err(TickConfigError(
                "health-check globs must be non-empty".to_string(),
            ));
        }
        if check.threshold_secs < 0 {
            return Err(TickConfigError(
                "health-check threshold_secs must be non-negative".to_string(),
            ));
        }
    }
    Ok(())
}

/// Emit canonical two-space JSON with stable field ordering.
///
/// Returns an error when a directly constructed model violates an interchange invariant.
pub fn config_to_json(config: &TickConfig) -> Result<String, TickConfigError> {
    validate_serializable(config)?;
    Ok(serde_json::to_string_pretty(&config_to_value(config))
        .expect("TickConfig contains only JSON-representable values"))
}

/// Emit YAML that round-trips through [`config_from_yaml`].
///
/// Returns an error when serialization fails or a directly constructed model violates an
/// interchange invariant.
pub fn config_to_yaml(config: &TickConfig) -> Result<String, TickConfigError> {
    validate_serializable(config)?;
    serde_norway::to_string(&config_to_value(config))
        .map_err(|error| TickConfigError(format!("cannot serialize YAML: {error}")))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_matches_stable_contract() {
        let config =
            config_from_json(r#"{"reminders":[{"name":"r","emit":{"skill":"s"}}]}"#).unwrap();
        assert_eq!(
            config_to_json(&config).unwrap(),
            "{\n  \"description\": \"\",\n  \"health_checks\": [],\n  \"reminders\": [\n    {\n      \"name\": \"r\",\n      \"cadence_secs\": 0,\n      \"requires_flags\": [],\n      \"gate\": null,\n      \"emit\": {\n        \"kind\": \"action\",\n        \"title\": \"\",\n        \"skill\": \"s\",\n        \"fields\": {}\n      }\n    }\n  ]\n}"
        );
    }

    #[test]
    fn minimal_defaults_and_note_without_skill() {
        let config =
            config_from_json(r#"{"reminders":[{"name":"n","emit":{"kind":"note","title":"hi"}}]}"#)
                .unwrap();
        let reminder = &config.reminders[0];
        assert_eq!(reminder.emit.kind, EmitKind::Note);
        assert_eq!(reminder.cadence_secs, 0);
        assert!(reminder.gate.is_none());
    }

    #[test]
    fn dependency_edges_round_trip_and_reject_invalid_graphs() {
        let text = r#"{
          "reminders": [
            {"name":"foundation","gate":{"cmd":"f","parallel":true},"emit":{"skill":"s"}},
            {"name":"dependent","depends_on":["foundation"],"gate":{"cmd":"d"},"emit":{"skill":"s"}}
          ]
        }"#;
        let config = config_from_json(text).unwrap();
        assert!(config.reminders[0].gate.as_ref().unwrap().parallel);
        assert_eq!(config.reminders[1].depends_on, vec!["foundation"]);
        assert_eq!(
            config_from_json(&config_to_json(&config).unwrap()).unwrap(),
            config
        );

        for invalid in [
            r#"{"reminders":[{"name":"a","depends_on":["missing"],"emit":{"skill":"s"}}]}"#,
            r#"{"reminders":[{"name":"a","depends_on":["a"],"emit":{"skill":"s"}}]}"#,
            r#"{"reminders":[{"name":"a","depends_on":["a","a"],"emit":{"skill":"s"}}]}"#,
            r#"{"reminders":[{"name":"a","depends_on":["b"],"emit":{"skill":"s"}},{"name":"b","depends_on":["a"],"emit":{"skill":"s"}}]}"#,
        ] {
            assert!(config_from_json(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn strict_errors_cover_all_nested_shapes() {
        let bad = [
            "not json",
            "[]",
            r#"{"reminders":"x"}"#,
            r#"{"reminders":null}"#,
            r#"{"health_checks":null}"#,
            r#"{"reminders":[{"emit":{"skill":"s"}}]}"#,
            r#"{"reminders":[{"name":"r"}]}"#,
            r#"{"reminders":[{"name":"r","emit":{"kind":"action"}}]}"#,
            r#"{"reminders":[{"name":"r","requires_flags":[1],"emit":{"skill":"s"}}]}"#,
            r#"{"reminders":[{"name":"r","requires_flags":null,"emit":{"skill":"s"}}]}"#,
            r#"{"reminders":[{"name":"r","emit":{"skill":"s","fields":null}}]}"#,
            r#"{"health_checks":[{"name":"h"}]}"#,
        ];
        for document in bad {
            assert!(config_from_json(document).is_err(), "accepted {document:?}");
        }
    }

    #[test]
    fn hardened_schema_rejects_unknown_fields_at_every_level() {
        let documents = [
            r#"{"surprise":1}"#,
            r#"{"reminders":[{"name":"r","surprise":1,"emit":{"skill":"s"}}]}"#,
            r#"{"reminders":[{"name":"r","emit":{"skill":"s","surprise":1}}]}"#,
            r#"{"reminders":[{"name":"r","gate":{"cmd":"true","surprise":1},"emit":{"skill":"s"}}]}"#,
            r#"{"health_checks":[{"name":"h","glob":"*","surprise":1}]}"#,
        ];
        for document in documents {
            let error = config_from_json(document).unwrap_err();
            assert!(error.0.contains("unknown field"), "{error}");
        }
    }

    #[test]
    fn hardened_schema_rejects_empty_duplicate_and_negative_values() {
        let documents = [
            r#"{"reminders":[{"name":" ","emit":{"skill":"s"}}]}"#,
            r#"{"reminders":[{"name":"has space","emit":{"skill":"s"}}]}"#,
            r#"{"reminders":[{"name":"has=equals","emit":{"skill":"s"}}]}"#,
            r#"{"reminders":[{"name":"__tick_hub_internal__.collision","emit":{"skill":"s"}}]}"#,
            r#"{"reminders":[{"name":"r","emit":{"skill":"  "}}]}"#,
            r#"{"reminders":[{"name":"r","cadence_secs":-1,"emit":{"skill":"s"}}]}"#,
            r#"{"reminders":[{"name":"r","emit":{"skill":"s"}},{"name":"r","emit":{"kind":"note"}}]}"#,
            r#"{"health_checks":[{"name":" ","glob":"*"}]}"#,
            r#"{"health_checks":[{"name":"h","glob":" "}]}"#,
            r#"{"health_checks":[{"name":"h","glob":"*","threshold_secs":-1}]}"#,
            r#"{"health_checks":[{"name":"h","glob":"*"},{"name":"h","glob":"/tmp/*"}]}"#,
        ];
        for document in documents {
            assert!(config_from_json(document).is_err(), "accepted {document}");
        }
    }

    #[test]
    fn yaml_rejects_non_string_keys_and_treats_signed_prefixed_ints_as_numbers() {
        let documents = [
            "1: value\n",
            "description: first\ndescription: second\n",
            "description: &base first\nother: &map {description: second}\n<<: *map\n",
            "reminders:\n- name: r\n  emit:\n    kind: note\n    fields: {1: value}\n",
            "description: -0xF0\n",
            "reminders:\n- name: r\n  emit: {kind: note, title: +0o7}\n",
        ];
        for document in documents {
            assert!(config_from_yaml(document).is_err(), "accepted {document:?}");
        }
    }

    #[test]
    fn json_rejects_duplicate_keys_at_any_depth() {
        let documents = [
            r#"{"description":"first","description":"second"}"#,
            r#"{"reminders":[{"name":"r","name":"other","emit":{"kind":"note"}}]}"#,
            r#"{"reminders":[{"name":"r","emit":{"kind":"note","fields":{"x":"1","x":"2"}}}]}"#,
        ];
        for document in documents {
            let error = config_from_json(document).unwrap_err();
            assert!(error.0.contains("duplicate object key"), "{error}");
        }
    }

    #[test]
    fn yaml_emitter_quotes_strings_that_resemble_signed_prefixed_ints() {
        let config = TickConfig {
            description: "-0xF0".into(),
            reminders: vec![Reminder::new("r", Emit::note("+0o7"))],
            health_checks: Vec::new(),
        };
        let yaml = config_to_yaml(&config).unwrap();
        let round_trip = config_from_yaml(&yaml).unwrap();
        assert_eq!(round_trip.description, "-0xF0");
        assert_eq!(round_trip.reminders[0].emit.title, "+0o7");
        assert!(yaml.contains("'-0xF0'") || yaml.contains("\"-0xF0\""));
        assert!(yaml.contains("'+0o7'") || yaml.contains("\"+0o7\""));
    }

    #[test]
    fn yaml_norway_tokens_stay_strings_and_round_trip() {
        let config =
            config_from_yaml("reminders:\n  - name: r\n    emit: {kind: note, title: no}\n")
                .unwrap();
        assert_eq!(config.reminders[0].emit.title, "no");
        let yaml = config_to_yaml(&config).unwrap();
        let round_trip = config_from_yaml(&yaml).unwrap();
        assert_eq!(
            config_to_json(&round_trip).unwrap(),
            config_to_json(&config).unwrap()
        );
    }

    #[test]
    fn yaml_core_scalar_matrix_matches_strict_string_fields() {
        for token in [
            "no",
            "yes",
            "on",
            "off",
            "0123",
            "1_000",
            "1:20",
            "2026-08-05",
        ] {
            let document =
                format!("reminders:\n- name: r\n  emit: {{kind: note, title: {token}}}\n");
            let config = config_from_yaml(&document).unwrap_or_else(|error| {
                panic!("plain token {token:?} should remain a string: {error}")
            });
            assert_eq!(config.reminders[0].emit.title, token);
        }
        for token in [
            "0o7", "+0xF0", "-0b10", "123", "1.5", "1e2", "true", "null", ".inf", ".nan",
        ] {
            let document =
                format!("reminders:\n- name: r\n  emit: {{kind: note, title: {token}}}\n");
            assert!(
                config_from_yaml(&document).is_err(),
                "numeric/non-string token {token:?} was accepted as a string"
            );
        }
    }

    #[test]
    fn reserved_merge_key_is_rejected_by_readers_and_writers() {
        assert!(config_from_json(
            r#"{"reminders":[{"name":"r","emit":{"kind":"note","fields":{"<<":"x"}}}]}"#,
        )
        .is_err());
        assert!(config_from_yaml(
            "reminders:\n- name: r\n  emit:\n    kind: note\n    fields: {'<<': x}\n",
        )
        .is_err());
        let mut emit = Emit::note("n");
        emit.fields.insert("<<".into(), "x".into());
        let config = TickConfig {
            reminders: vec![Reminder::new("r", emit)],
            ..TickConfig::default()
        };
        assert!(config_to_json(&config).is_err());
        assert!(config_to_yaml(&config).is_err());

        let invalid_name = TickConfig {
            reminders: vec![Reminder::new("not persistable", Emit::note("n"))],
            ..TickConfig::default()
        };
        assert!(config_to_json(&invalid_name).is_err());
        assert!(config_to_yaml(&invalid_name).is_err());

        let reserved_name = TickConfig {
            reminders: vec![Reminder::new(
                "__tick_hub_internal__.collision",
                Emit::note("n"),
            )],
            ..TickConfig::default()
        };
        assert!(config_to_json(&reserved_name).is_err());
        assert!(config_to_yaml(&reserved_name).is_err());

        let mut invalid_action = Reminder::new("r", Emit::action(" ", "invalid"));
        invalid_action.cadence_secs = -1;
        let invalid_model = TickConfig {
            reminders: vec![invalid_action],
            health_checks: vec![HealthCheck::new("h", " ", -1)],
            ..TickConfig::default()
        };
        assert!(config_to_json(&invalid_model).is_err());
    }
}
