//! Nested `agentctl-session/v2` and `agentctl-session/v3` registry rows.
//!
//! These rows keep launch intent once, under a canonical `launch` object, instead of beside the
//! mutable fields. A row is accepted only with exactly its schema's key set and launch contract,
//! decoded into the ordinary [`AgentRecord`], and written back in the same nested form. A
//! write-back refuses a record whose launch intent changed, because the nested form keeps that
//! intent in one place only. `anchor` is the one operation that rewrites a nested row as flat
//! schema 1, since anchors exist only there.

// Every check, message and rule here matches `AgentRecord._from_value`,
// `_normalize_nested_storage` and `_nested_document` in the other edition's
// py/agentctl/subagents.py, so both editions accept, refuse and rewrite the same rows.

use serde_json::Map;

use super::*;

const SESSION_V2: &str = "agentctl-session/v2";
const SESSION_V3: &str = "agentctl-session/v3";
const LAUNCH_SCHEMAS: [&str; 2] = ["agentctl-launch/v1", "agentctl-launch/v2"];
/// Keys every nested row carries.
const COMMON_FIELDS: [&str; 17] = [
    "schema",
    "name",
    "token",
    "created_at",
    "lifecycle",
    "launch",
    "workspace_id",
    "tab_id",
    "pane_id",
    "startup_warning",
    "effective_reasoning_effort",
    "error",
    "paused",
    "pane_reported_by_agentctl",
    "foreign_shell_identity",
    "runner_identity",
    "extensions",
];
/// Keys only a v2 row carries: flat session and goal fields.
const V2_FIELDS: [&str; 9] = [
    "session_agent",
    "session_value",
    "goal",
    "goal_delivery",
    "goal_session_id",
    "goal_command",
    "goal_messages",
    "goal_message_id",
    "custom_process_identity",
];
/// Keys only a v3 row carries: one native session object and one goal object.
const V3_FIELDS: [&str; 3] = ["native_session", "goal", "custom_process_identity"];
/// Keys of a v1 launch specification; v2 adds `permission_mode`.
const LAUNCH_FIELDS: [&str; 14] = [
    "schema",
    "harness",
    "cwd",
    "adapter",
    "mode",
    "backend",
    "model",
    "resume",
    "profile",
    "argv",
    "environment_names",
    "runtime_home",
    "runtime_ownership",
    "executable",
];
const LIFECYCLES: [&str; 6] = [
    "starting",
    "running",
    "stopping",
    "stopped",
    "launch_failed",
    "adopt_failed",
];

/// What a decoded nested row keeps beside the flat fields, to write itself back unchanged.
///
/// A write-back refreshes every other key of the row from the record, so the schema, the launch
/// specification, the runner identity (always null for an interactive row) and the extensions
/// are all the original row contributes.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct NestedRecord {
    schema: String,
    launch: Map<String, Value>,
    extensions: Map<String, Value>,
    /// How the native session was bound: `observed`, `asserted`, or unknown.
    session_source: Option<String>,
}

/// Decode `document` if it is a nested v2 or v3 row, or return `None` so the caller decodes it
/// as flat schema 1. The caller still runs [`AgentRecord::validate_loaded`] on the decoded
/// record, the checks every flat row passes too.
pub(super) fn decode(document: &Value, path: &Path) -> Option<Result<AgentRecord>> {
    let source = document.as_object()?;
    let schema = source
        .get("schema")
        .and_then(Value::as_str)
        .filter(|schema| [SESSION_V2, SESSION_V3].contains(schema))?;
    Some(decode_nested(source, schema, path))
}

/// [`decode`] for raw record bytes; `None` also when the bytes are not JSON, which the
/// caller's flat decoder reports in its own words.
pub(super) fn decode_bytes(content: &[u8], path: &Path) -> Option<Result<AgentRecord>> {
    decode(&serde_json::from_slice(content).ok()?, path)
}

fn decode_nested(source: &Map<String, Value>, schema: &str, path: &Path) -> Result<AgentRecord> {
    let present = |key: &str| source.get(key).is_some_and(|value| !value.is_null());
    let session_source = if schema == SESSION_V3 {
        source
            .get("native_session")
            .and_then(Value::as_object)
            .and_then(|native| native.get("source"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    } else if present("session_value") {
        Some("observed".to_owned())
    } else if present("goal_session_id") {
        Some("asserted".to_owned())
    } else {
        None
    };
    let normalized = normalize(source, schema, path)?;
    let mut record: AgentRecord = serde_json::from_value(Value::Object(normalized))
        .map_err(|error| fail(format!("invalid agent record {}: {error}", path.display())))?;
    let object = |key: &str| {
        source
            .get(key)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };
    record.nested = Some(NestedRecord {
        schema: schema.to_owned(),
        launch: object("launch"),
        extensions: object("extensions"),
        session_source,
    });
    Ok(record)
}

/// `[a-z][a-z0-9-]{0,63}`: a Herdr agent kind.
fn kind_pattern(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// A nonempty string with no NUL.
fn nonempty_text(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|text| !text.is_empty() && !text.contains('\0'))
}

fn absolute(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|text| Path::new(text).is_absolute())
}

/// Decode a v2/v3 row at the boundary while leaving its launch data canonical.
fn normalize(
    document: &Map<String, Value>,
    schema: &str,
    path: &Path,
) -> Result<Map<String, Value>> {
    let path = path.display();
    let invalid = |what: &str| fail(format!("{what}: {path}"));
    let mut expected: BTreeSet<&str> = COMMON_FIELDS.into_iter().collect();
    expected.extend(if schema == SESSION_V2 {
        &V2_FIELDS[..]
    } else {
        &V3_FIELDS[..]
    });
    if document.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected {
        return Err(fail(format!(
            "invalid agent record {schema} fields: {path}"
        )));
    }
    let Some(launch) = document.get("launch").and_then(Value::as_object) else {
        return Err(invalid("invalid agent record launch specification"));
    };
    let mut launch_fields: BTreeSet<&str> = LAUNCH_FIELDS.into_iter().collect();
    match launch.get("schema").and_then(Value::as_str) {
        Some("agentctl-launch/v2") => {
            launch_fields.insert("permission_mode");
        }
        Some(value) if LAUNCH_SCHEMAS.contains(&value) => {}
        _ => return Err(invalid("invalid agent record launch schema")),
    }
    if launch.keys().map(String::as_str).collect::<BTreeSet<_>>() != launch_fields {
        return Err(invalid("invalid agent record launch fields"));
    }
    let lifecycle = document.get("lifecycle").and_then(Value::as_str);
    if !lifecycle.is_some_and(|value| LIFECYCLES.contains(&value)) {
        return Err(invalid("invalid agent record lifecycle"));
    }
    let null = Value::Null;
    let field = |key: &str| launch.get(key).unwrap_or(&null);
    let contract = || invalid("invalid nested interactive launch contract");
    let (Some(harness), Some(adapter)) = (field("harness").as_str(), field("adapter").as_str())
    else {
        return Err(contract());
    };
    let ownership = field("runtime_ownership").as_str();
    let profile = field("profile");
    if !kind_pattern(harness)
        || !absolute(field("cwd"))
        || !matches!(
            adapter,
            "herdr" | "herdr-pane" | "herdr-foreign" | "herdr-relay"
        )
        || field("mode").as_str() != Some("interactive")
        || field("backend").as_str() != Some("herdr")
        || (!profile.is_null() && !nonempty_text(profile))
        || !field("permission_mode").is_null()
        || !field("runtime_home").is_null()
    {
        return Err(contract());
    }
    if (adapter == "herdr-foreign" && ownership != Some("foreign"))
        || (adapter != "herdr-foreign" && ownership != Some("owned"))
        || (adapter == "herdr-pane" && harness != "muse")
        || (adapter == "herdr-relay" && !RELAY_HARNESSES.contains(&harness))
    {
        return Err(invalid("inconsistent launch ownership or adapter"));
    }
    for field_name in ["model", "resume"] {
        let value = field(field_name);
        if !value.is_null() && !nonempty_text(value) {
            return Err(fail(format!(
                "invalid agent record launch {field_name}: {path}"
            )));
        }
    }
    let argv = field("argv").as_array();
    let environment_names = field("environment_names").as_array();
    let (Some(argv), Some(environment_names)) = (argv, environment_names) else {
        return Err(invalid("invalid agent record launch vector"));
    };
    if !argv.iter().all(nonempty_text)
        || !environment_names
            .iter()
            .all(|item| item.as_str().is_some_and(valid_environment_name))
    {
        return Err(invalid("invalid agent record launch vector"));
    }
    let argv: Vec<&str> = argv.iter().filter_map(Value::as_str).collect();
    let executable = field("executable");
    if !executable.is_null()
        && !executable.as_object().is_some_and(|identity| {
            identity.keys().map(String::as_str).collect::<BTreeSet<_>>()
                == BTreeSet::from(["path", "device", "inode"])
                && identity.get("path").is_some_and(absolute)
                && ["device", "inode"].iter().all(|key| {
                    identity
                        .get(*key)
                        .and_then(Value::as_u64)
                        .is_some_and(|value| value > 0)
                })
        })
    {
        return Err(invalid("invalid agent record launch executable identity"));
    }
    let expected_program = executable
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(harness);
    if (adapter == "herdr-foreign" && !argv.is_empty())
        || (adapter != "herdr-foreign" && argv.first() != Some(&expected_program))
    {
        return Err(invalid("invalid agent record launch argv"));
    }
    if matches!(adapter, "herdr" | "herdr-pane" | "herdr-relay") {
        let structured = harness_arguments(
            harness,
            field("model").as_str(),
            field("resume").as_str(),
            &[],
        )
        .map_err(|error| fail(format!("invalid nested launch presets in {path}: {error}")))?;
        let end = (1 + structured.len()).min(argv.len());
        if argv[1.min(argv.len())..end] != structured {
            return Err(invalid("nested launch argv contradicts model or resume"));
        }
    }
    if !document.get("runner_identity").is_none_or(Value::is_null) {
        return Err(invalid(
            "interactive nested record cannot contain runner identity",
        ));
    }
    if matches!(adapter, "herdr-pane" | "herdr-relay")
        && matches!(lifecycle, Some("running" | "stopping" | "stopped"))
        && document
            .get("custom_process_identity")
            .is_none_or(Value::is_null)
    {
        return Err(invalid(
            "active nested Muse record has no runtime process identity",
        ));
    }
    let extensions_valid = document
        .get("extensions")
        .and_then(Value::as_object)
        .is_some_and(|extensions| {
            extensions.keys().all(|key| {
                !expected.contains(key.as_str()) && !launch_fields.contains(key.as_str())
            })
        });
    if !extensions_valid {
        return Err(invalid("invalid agent record extensions"));
    }
    let mut normalized: Map<String, Value> = document
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "schema" | "launch" | "extensions" | "native_session"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    normalized.insert("schema".to_owned(), json!(1));
    for key in [
        "harness",
        "cwd",
        "adapter",
        "mode",
        "backend",
        "model",
        "resume",
        "runtime_home",
        "profile",
    ] {
        normalized.insert(key.to_owned(), field(key).clone());
    }
    normalized.insert("arguments".to_owned(), json!(argv[1.min(argv.len())..]));
    normalized.insert(
        "environment_names".to_owned(),
        Value::Array(environment_names.clone()),
    );
    if schema == SESSION_V3 {
        match document.get("native_session").unwrap_or(&null) {
            Value::Null => {
                for key in ["session_agent", "session_value", "goal_session_id"] {
                    normalized.insert(key.to_owned(), Value::Null);
                }
            }
            Value::Object(native)
                if native.keys().map(String::as_str).collect::<BTreeSet<_>>()
                    == BTreeSet::from(["schema", "agent", "value", "source"])
                    && native.get("schema").and_then(Value::as_str)
                        == Some("agentctl-native-session/v1")
                    && matches!(
                        native.get("source").and_then(Value::as_str),
                        Some("observed" | "asserted")
                    )
                    && native.get("agent").and_then(Value::as_str) == Some(harness)
                    && native
                        .get("value")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.is_empty()) =>
            {
                normalized.insert("session_agent".to_owned(), native["agent"].clone());
                normalized.insert("session_value".to_owned(), native["value"].clone());
                normalized.insert("goal_session_id".to_owned(), native["value"].clone());
                normalized.insert("native_session".to_owned(), Value::Object(native.clone()));
            }
            _ => return Err(invalid("invalid agent record native session")),
        }
        let goal = document
            .get("goal")
            .and_then(Value::as_object)
            .filter(|goal| {
                goal.keys().map(String::as_str).collect::<BTreeSet<_>>()
                    == BTreeSet::from(["schema", "objective", "message_id", "native_command"])
                    && goal.get("schema").and_then(Value::as_str) == Some("agentctl-goal/v1")
            });
        let Some(goal) = goal else {
            return Err(invalid("invalid agent record goal state"));
        };
        normalized.insert("goal".to_owned(), goal["objective"].clone());
        normalized.insert("goal_delivery".to_owned(), Value::Null);
        normalized.insert("goal_command".to_owned(), goal["native_command"].clone());
        normalized.insert("goal_messages".to_owned(), json!({}));
        normalized.insert("goal_message_id".to_owned(), goal["message_id"].clone());
    } else {
        let observed = normalized.get("session_value").unwrap_or(&null);
        let asserted = normalized.get("goal_session_id").unwrap_or(&null);
        if !observed.is_null() && !asserted.is_null() && observed != asserted {
            return Err(fail(format!(
                "contradictory native session identities in {path}"
            )));
        }
        if !observed.is_null()
            && (normalized.get("session_agent").and_then(Value::as_str) != Some(harness)
                || !observed.as_str().is_some_and(|value| !value.is_empty()))
        {
            return Err(fail(format!("invalid native session identity in {path}")));
        }
    }
    Ok(normalized)
}

/// Sort every object's keys, so the written row reads alike whatever map order this build uses.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let ordered: BTreeMap<String, Value> = object
                .into_iter()
                .map(|(key, value)| (key, sorted(value)))
                .collect();
            Value::Object(ordered.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

impl NestedRecord {
    /// Update the mutable fields while retaining one canonical nested launch specification.
    fn document(&self, record: &AgentRecord) -> Result<Value> {
        let launch = &self.launch;
        let projection = [
            ("harness", json!(record.harness)),
            ("cwd", json!(record.cwd)),
            ("adapter", json!(record.adapter)),
            ("mode", json!(record.mode)),
            ("backend", json!(record.backend)),
            ("model", json!(record.model)),
            ("resume", json!(record.resume)),
            ("runtime_home", json!(record.runtime_home)),
        ];
        let arguments_unchanged =
            launch
                .get("argv")
                .and_then(Value::as_array)
                .is_some_and(|argv| {
                    argv.iter()
                        .skip(1)
                        .map(Value::as_str)
                        .eq(record.arguments.iter().map(|item| Some(item.as_str())))
                });
        if projection
            .iter()
            .any(|(key, value)| launch.get(*key).unwrap_or(&Value::Null) != value)
            || !arguments_unchanged
        {
            return Err(fail(
                "decoded nested launch intent changed; migrate through a canonical writer",
            ));
        }
        let mut document = json!({
            "schema": self.schema,
            "launch": launch,
            "runner_identity": null,
            "extensions": self.extensions,
            "name": record.name,
            "token": record.token,
            "created_at": record.created_at,
            "lifecycle": record.lifecycle,
            "workspace_id": record.workspace_id,
            "tab_id": record.tab_id,
            "pane_id": record.pane_id,
            "startup_warning": record.startup_warning,
            "effective_reasoning_effort": record.effective_reasoning_effort,
            "error": record.error,
            "paused": record.paused,
            "pane_reported_by_agentctl": record.pane_reported_by_agentctl,
            "foreign_shell_identity": record.foreign_shell_identity,
            "custom_process_identity": record.custom_process_identity,
        });
        let fields = document.as_object_mut().expect("nested row object");
        if self.schema == SESSION_V2 {
            for (key, value) in [
                ("session_agent", json!(record.session_agent)),
                ("session_value", json!(record.session_value)),
                ("goal", json!(record.goal)),
                ("goal_delivery", json!(record.goal_delivery)),
                ("goal_session_id", json!(record.goal_session_id)),
                ("goal_command", json!(record.goal_command)),
                ("goal_messages", json!(record.goal_messages)),
                ("goal_message_id", json!(record.goal_message_id)),
            ] {
                fields.insert(key.to_owned(), value);
            }
        } else {
            let native = record.session_value.as_ref().map(|value| {
                json!({
                    "schema": "agentctl-native-session/v1",
                    "agent": record.session_agent.as_deref().unwrap_or(&record.harness),
                    "value": value,
                    "source": self.session_source.as_deref().unwrap_or("observed"),
                })
            });
            fields.insert("native_session".to_owned(), json!(native));
            fields.insert(
                "goal".to_owned(),
                json!({
                    "schema": "agentctl-goal/v1",
                    "objective": record.goal,
                    "message_id": record.goal_message_id,
                    "native_command": record.goal_command,
                }),
            );
        }
        Ok(sorted(document))
    }
}

impl AgentRecord {
    /// The row this record is stored as: its nested v2/v3 form when it was decoded from one,
    /// otherwise flat schema 1.
    pub(super) fn document(&self) -> Result<Value> {
        match &self.nested {
            Some(nested) => nested.document(self),
            None => Ok(json!(self)),
        }
    }

    /// [`Self::document`] as pretty JSON bytes without a trailing newline; a flat record keeps
    /// its field order.
    pub(super) fn document_pretty_bytes(&self) -> Result<Vec<u8>> {
        match &self.nested {
            Some(nested) => serde_json::to_vec_pretty(&nested.document(self)?),
            None => serde_json::to_vec_pretty(self),
        }
        .map_err(|error| fail(format!("cannot serialize agent record: {error}")))
    }

    /// Was the native session asserted by an operator rather than reported by the pane? Only a
    /// nested row can say so; an asserted session never pins routing or counts as an anchor.
    pub(super) fn session_asserted(&self) -> bool {
        self.nested
            .as_ref()
            .is_some_and(|nested| nested.session_source.as_deref() == Some("asserted"))
    }

    /// The native session the pane reported, which routing and recipient checks may rely on.
    pub(super) fn observed_session_value(&self) -> Option<&str> {
        self.session_value
            .as_deref()
            .filter(|_| !self.session_asserted())
    }

    /// The `(session_agent, session_value)` pair a delivery target pins.
    pub(super) fn routing_session(&self) -> (Option<String>, Option<String>) {
        if self.session_asserted() {
            (None, None)
        } else {
            (self.session_agent.clone(), self.session_value.clone())
        }
    }

    /// Record an explicitly bound native session. A v3 row keeps it as its one asserted native
    /// session; other rows keep it as the asserted goal session only.
    pub(super) fn bind_asserted_session(&mut self, session_id: &str) {
        match self.nested.as_mut() {
            Some(nested) if nested.schema == SESSION_V3 => {
                self.session_agent = Some(self.harness.clone());
                self.session_value = Some(session_id.to_owned());
                self.goal_session_id = Some(session_id.to_owned());
                nested.session_source = Some("asserted".to_owned());
            }
            _ => self.goal_session_id = Some(session_id.to_owned()),
        }
    }

    /// Turn a nested record into flat schema 1, returning the schema it had. An asserted
    /// session stays asserted: it moves to `goal_session_id` and never becomes observed.
    pub(super) fn migrate_nested_to_flat(&mut self) -> Option<String> {
        let nested = self.nested.take()?;
        if nested.session_source.as_deref() == Some("asserted") {
            self.goal_session_id = self.session_value.take().or(self.goal_session_id.take());
            self.session_agent = None;
        }
        Some(nested.schema)
    }

    /// Rename keeps its journal and history in flat schema 1 only.
    pub(super) fn refuse_nested_rename(&self) -> Result<()> {
        if self.nested.is_some() {
            return Err(fail(format!(
                "agent '{name}' uses a nested record format; run `agentctl anchor {name}`, which rewrites it as schema 1, then rename",
                name = self.name
            )));
        }
        Ok(())
    }
}
