//! Typed, captured configuration for shadow policy evaluation.

use std::fs::{self, OpenOptions};
use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canonical::canonical_sha256;
use crate::replay::validate_name;
use crate::ObserverError;

const INPUT_BYTES_LIMIT: u64 = 16 * 1024 * 1024;
// chrono stores a TimeDelta as milliseconds plus sub-millisecond nanoseconds.
// Its infallible `Duration::seconds` constructor panics above this bound.
const MAX_TIMEDELTA_SECONDS: u64 = i64::MAX as u64 / 1_000;

/// Versioned policy configuration consumed only while rebuilding an index.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShadowConfig {
    pub(crate) schema: u64,
    pub(crate) machine: String,
    /// A slot must be older than this duration as well as past its heartbeat TTL.
    pub(crate) minimum_stale_seconds: u64,
    /// Maximum number of eligible entries returned by a pressure plan.
    pub(crate) max_plan_slots: usize,
    /// Maximum accepted age of a completed evidence census at evaluation time.
    pub(crate) evidence_max_age_seconds: u64,
    /// Maximum wall-clock duration covered by one census.
    pub(crate) maximum_census_seconds: u64,
    /// Maximum tolerated positive wall-clock skew of the evidence producer.
    pub(crate) maximum_future_skew_seconds: u64,
}

/// A parsed JSON input and the digest of its canonical bytes.
pub(crate) struct Digested<T> {
    pub(crate) value: T,
    pub(crate) sha256: String,
    pub(crate) canonical_json: String,
}

impl ShadowConfig {
    pub(crate) fn load(path: &Path) -> Result<Digested<Self>, ObserverError> {
        let loaded: Digested<Self> = load_typed_json(path, "shadow policy configuration")?;
        loaded.value.validate()?;
        Ok(loaded)
    }

    pub(crate) fn validate(&self) -> Result<(), ObserverError> {
        if self.schema != 1 {
            return Err(ObserverError::invalid(format!(
                "unsupported shadow policy configuration schema {}",
                self.schema
            )));
        }
        validate_name(&self.machine, "shadow policy machine")?;
        if self.max_plan_slots == 0 || self.max_plan_slots > 10_000 {
            return Err(ObserverError::invalid(
                "shadow policy max_plan_slots must be between 1 and 10000",
            ));
        }
        if self.evidence_max_age_seconds == 0
            || self.maximum_census_seconds == 0
            || self.maximum_future_skew_seconds > self.evidence_max_age_seconds
            || self.evidence_max_age_seconds > MAX_TIMEDELTA_SECONDS
            || self.maximum_census_seconds > MAX_TIMEDELTA_SECONDS
            || self.maximum_future_skew_seconds > MAX_TIMEDELTA_SECONDS
        {
            return Err(ObserverError::invalid(
                "shadow policy evidence age and census bounds must be positive, all freshness durations must be representable, and future skew must not exceed maximum evidence age",
            ));
        }
        Ok(())
    }
}

pub(crate) fn load_typed_json<T: DeserializeOwned>(
    path: &Path,
    label: &str,
) -> Result<Digested<T>, ObserverError> {
    load_typed_json_after_inspect(path, label, || {})
}

fn load_typed_json_after_inspect<T: DeserializeOwned>(
    path: &Path,
    label: &str,
    after_inspect: impl FnOnce(),
) -> Result<Digested<T>, ObserverError> {
    let path_before = fs::symlink_metadata(path).map_err(|error| {
        ObserverError::with_source(format!("cannot inspect {label} {}", path.display()), error)
    })?;
    if path_before.file_type().is_symlink() || !path_before.is_file() || path_before.nlink() != 1 {
        return Err(ObserverError::invalid(format!(
            "{label} is not a singly linked regular file: {}",
            path.display()
        )));
    }
    if path_before.len() > INPUT_BYTES_LIMIT {
        return Err(ObserverError::invalid(format!(
            "{label} exceeds {INPUT_BYTES_LIMIT} bytes: {}",
            path.display()
        )));
    }
    after_inspect();
    let mut file = OpenOptions::new()
        .read(true)
        // A regular input can be replaced after the path inspection.  Opening
        // a replacement FIFO without O_NONBLOCK would wait forever for a
        // writer instead of reaching the fd identity/type check below.
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            ObserverError::with_source(format!("cannot open {label} {}", path.display()), error)
        })?;
    let before = file.metadata().map_err(|error| {
        ObserverError::with_source(
            format!("cannot inspect open {label} {}", path.display()),
            error,
        )
    })?;
    if (path_before.dev(), path_before.ino()) != (before.dev(), before.ino())
        || !before.is_file()
        || before.nlink() != 1
        || before.len() > INPUT_BYTES_LIMIT
    {
        return Err(ObserverError::invalid(format!(
            "{label} changed while it was opened: {}",
            path.display()
        )));
    }
    let capacity = usize::try_from(before.len())
        .map_err(|_| ObserverError::invalid(format!("{label} is too large")))?;
    let mut contents = Vec::with_capacity(capacity);
    file.by_ref()
        .take(INPUT_BYTES_LIMIT + 1)
        .read_to_end(&mut contents)
        .map_err(|error| {
            ObserverError::with_source(format!("cannot read {label} {}", path.display()), error)
        })?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > INPUT_BYTES_LIMIT {
        return Err(ObserverError::invalid(format!(
            "{label} exceeds {INPUT_BYTES_LIMIT} bytes while being read: {}",
            path.display()
        )));
    }
    let after = file.metadata().map_err(|error| {
        ObserverError::with_source(
            format!("cannot re-inspect open {label} {}", path.display()),
            error,
        )
    })?;
    let path_after = fs::symlink_metadata(path).map_err(|error| {
        ObserverError::with_source(
            format!("cannot re-inspect {label} {}", path.display()),
            error,
        )
    })?;
    if (
        before.dev(),
        before.ino(),
        before.len(),
        before.mtime(),
        before.mtime_nsec(),
    ) != (
        after.dev(),
        after.ino(),
        after.len(),
        after.mtime(),
        after.mtime_nsec(),
    ) || (after.dev(), after.ino()) != (path_after.dev(), path_after.ino())
        || after.nlink() != 1
        || path_after.nlink() != 1
        || path_after.file_type().is_symlink()
        || !after.is_file()
        || contents.len() as u64 != after.len()
    {
        return Err(ObserverError::invalid(format!(
            "{label} changed while it was read: {}",
            path.display()
        )));
    }
    let raw: Value = serde_json::from_slice(&contents).map_err(|error| {
        ObserverError::with_source(format!("cannot parse {label} {}", path.display()), error)
    })?;
    let canonical_json = crate::canonical::canonical_json(&raw)?;
    let sha256 = canonical_sha256(&raw)?;
    let value = serde_json::from_value(raw).map_err(|error| {
        ObserverError::with_source(format!("invalid {label} {}", path.display()), error)
    })?;
    Ok(Digested {
        value,
        sha256,
        canonical_json,
    })
}

#[cfg(test)]
pub(crate) fn load_typed_json_with_post_inspect_hook<T: DeserializeOwned>(
    path: &Path,
    label: &str,
    after_inspect: impl FnOnce(),
) -> Result<Digested<T>, ObserverError> {
    load_typed_json_after_inspect(path, label, after_inspect)
}

pub(crate) fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) mod decimal_u64 {
    use std::fmt;

    use serde::de::Visitor;
    use serde::{Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        struct DecimalU64Visitor;

        impl<'de> Visitor<'de> for DecimalU64Visitor {
            type Value = u64;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a u64 or its canonical decimal string")
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<u64, E> {
                Ok(value)
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<u64, E> {
                let parsed = value.parse::<u64>().map_err(E::custom)?;
                if parsed.to_string() != value {
                    return Err(E::custom("non-canonical decimal u64"));
                }
                Ok(parsed)
            }
        }

        deserializer.deserialize_any(DecimalU64Visitor)
    }
}

pub(crate) mod optional_decimal_u64 {
    use std::fmt;

    use serde::de::Visitor;
    use serde::{Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(
        value: &Option<u64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_some(&value.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        struct OptionalDecimalU64Visitor;

        impl<'de> Visitor<'de> for OptionalDecimalU64Visitor {
            type Value = Option<u64>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("null, a u64, or its canonical decimal string")
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_some<D: Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                super::decimal_u64::deserialize(deserializer).map(Some)
            }
        }

        deserializer.deserialize_option(OptionalDecimalU64Visitor)
    }
}

pub(crate) mod decimal_u128 {
    use serde::Serializer;

    pub(crate) fn serialize<S: Serializer>(value: &u128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
}
