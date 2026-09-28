use serde::Deserialize;
use serde::Serialize;
use tracing::metadata::LevelFilter;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::Directive;

const VERSION: u32 = 2;
const TOOL: &str = "hermit-detcore-liteinst-v1";
pub const RUNTIME_IMAGE_FD_ENV: &str = "HERMIT_LITEINST_RUNTIME_IMAGE_FD";

#[derive(Debug)]
pub struct EffectiveFilter {
    directives: String,
    filter: EnvFilter,
}

impl EffectiveFilter {
    pub fn from_directives_lossy(raw: &str, level: LevelFilter) -> Self {
        let filter = EnvFilter::new(raw)
            .add_directive("tokio=debug".parse().expect("correct directive"))
            .add_directive(level.into());
        let mut accepted: Vec<_> = raw
            .split(',')
            .filter(|directive| !directive.is_empty())
            .filter(|directive| directive.parse::<Directive>().is_ok())
            .collect();
        let level_text = level.to_string();
        accepted.push("tokio=debug");
        accepted.push(&level_text);
        Self {
            directives: accepted.join(","),
            filter,
        }
    }

    pub fn into_filter(self) -> EnvFilter {
        self.filter
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    version: u32,
    tool: String,
    config_wire_fingerprint: String,
    log_filter: String,
}

pub fn encode(
    config_wire_fingerprint: &str,
    log_filter: &EffectiveFilter,
) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&Payload {
        version: VERSION,
        tool: TOOL.to_owned(),
        config_wire_fingerprint: config_wire_fingerprint.to_owned(),
        log_filter: log_filter.directives.clone(),
    })
}

pub fn decode(bytes: &[u8], expected_fingerprint: &str) -> Result<EffectiveFilter, String> {
    let payload: Payload = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid bootstrap payload: {error}"))?;
    if payload.version != VERSION {
        return Err("unsupported bootstrap payload version".to_owned());
    }
    if payload.tool != TOOL {
        return Err("bootstrap tool mismatch".to_owned());
    }
    if payload.config_wire_fingerprint != expected_fingerprint {
        return Err("bootstrap config fingerprint mismatch".to_owned());
    }
    let filter = EnvFilter::try_new(&payload.log_filter)
        .map_err(|error| format!("invalid bootstrap log filter: {error}"))?;
    Ok(EffectiveFilter {
        directives: payload.log_filter,
        filter,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use serde_json::json;

    use super::*;

    const FINGERPRINT: &str = "fixture-config-fingerprint";

    fn payload() -> Value {
        json!({
            "version": VERSION,
            "tool": TOOL,
            "config_wire_fingerprint": FINGERPRINT,
            "log_filter": "info,tokio=debug,detcore[work{task=7}]=trace",
        })
    }

    #[test]
    fn schema_and_filter_round_trip() {
        let raw = payload()["log_filter"].as_str().unwrap().to_owned();
        let filter = EffectiveFilter::from_directives_lossy(&raw, LevelFilter::WARN);
        let encoded = encode(FINGERPRINT, &filter).unwrap();
        let mut expected = payload();
        expected["log_filter"] = json!(format!("{raw},tokio=debug,warn"));
        assert_eq!(serde_json::from_slice::<Value>(&encoded).unwrap(), expected);
        let decoded = decode(&encoded, FINGERPRINT).unwrap();
        assert_eq!(decoded.directives, filter.directives);
        assert_eq!(encode(FINGERPRINT, &decoded).unwrap(), encoded);
    }

    #[test]
    fn preserves_accepted_spelling_and_override_order() {
        let raw = "warn,,detcore[work{task=1.0}]=info,tokio=off,broken=bogus,detcore[work{task=-1.0}]=debug,detcore[work{task=1e0}]=trace";
        let filter = EffectiveFilter::from_directives_lossy(raw, LevelFilter::WARN);
        let expected = "warn,detcore[work{task=1.0}]=info,tokio=off,detcore[work{task=-1.0}]=debug,detcore[work{task=1e0}]=trace,tokio=debug,warn";
        let encoded = encode(FINGERPRINT, &filter).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&encoded).unwrap()["log_filter"],
            expected
        );
        let decoded = decode(&encoded, FINGERPRINT).unwrap();
        assert_eq!(decoded.directives, expected);
        assert_eq!(encode(FINGERPRINT, &decoded).unwrap(), encoded);
    }

    #[test]
    fn refuses_wrong_version_tool_fingerprint_and_filter() {
        for (field, value, expected) in [
            ("version", json!(1), "unsupported bootstrap payload version"),
            ("tool", json!("other-tool"), "bootstrap tool mismatch"),
            (
                "config_wire_fingerprint",
                json!("other-config"),
                "bootstrap config fingerprint mismatch",
            ),
            (
                "log_filter",
                json!("detcore=bogus"),
                "invalid bootstrap log filter:",
            ),
        ] {
            let mut invalid = payload();
            invalid[field] = value;
            let error = decode(&serde_json::to_vec(&invalid).unwrap(), FINGERPRINT).unwrap_err();
            assert!(error.starts_with(expected), "{field}: {error}");
        }
    }

    #[test]
    fn refuses_missing_unknown_and_wrong_typed_fields() {
        for field in ["version", "tool", "config_wire_fingerprint", "log_filter"] {
            let mut missing = payload();
            missing.as_object_mut().unwrap().remove(field);
            assert!(decode(&serde_json::to_vec(&missing).unwrap(), FINGERPRINT).is_err());
            let mut wrong_type = payload();
            wrong_type[field] = json!(false);
            assert!(decode(&serde_json::to_vec(&wrong_type).unwrap(), FINGERPRINT).is_err());
        }
        let mut unknown = payload();
        unknown["extra"] = json!(0);
        assert!(decode(&serde_json::to_vec(&unknown).unwrap(), FINGERPRINT).is_err());
    }

    #[test]
    fn refuses_old_payload_duplicate_fields_trailing_data_and_invalid_utf8() {
        let old = json!({"tool": TOOL, "config_wire_fingerprint": FINGERPRINT});
        assert!(decode(&serde_json::to_vec(&old).unwrap(), FINGERPRINT).is_err());
        let valid = serde_json::to_string(&payload()).unwrap();
        let duplicate = format!("{{\"version\":2,{}", &valid[1..]);
        assert!(decode(duplicate.as_bytes(), FINGERPRINT).is_err());
        assert!(decode(format!("{valid} null").as_bytes(), FINGERPRINT).is_err());
        assert!(decode(b"\xff", FINGERPRINT).is_err());
        assert!(decode(b"{", FINGERPRINT).is_err());
    }
}
