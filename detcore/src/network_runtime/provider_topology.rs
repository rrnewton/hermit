//! Authenticated package topology metadata, never event or descriptor custody.

use std::io;

use serde::Deserialize;
use serde::Serialize;

/// The topology selected by a matched maintained package. Nonclassic digests
/// cover the complete canonical compiled contract, including every role.
/// Deserializing this value does not open a provider or recreate a broker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum ProviderTopology {
    /// Historical independent classic perf-event attachments.
    #[serde(rename = "classic-v40")]
    ClassicV40,
    /// One named event containing the exact grouped-v1 physical definitions.
    #[serde(rename = "grouped-v1")]
    GroupedV1 {
        /// SHA256 of canonical JSON for the complete matched compiled contract.
        contract_sha256: [u8; 32],
    },
    /// Link-owned fentry/fexit and KPROBE_MULTI/session observations only.
    /// No tracefs definition, perf-event kprobe link or grouped broker exists.
    #[serde(rename = "ftrace-v1")]
    FtraceV1 {
        /// SHA256 of canonical JSON for the complete matched compiled contract.
        contract_sha256: [u8; 32],
    },
}

// Serde's internally tagged unit variant discards extra fields even with
// deny_unknown_fields. Decode through an empty struct variant so the actual
// map visitor rejects every unexpected key; retain the public unit and its
// existing serialized wire shape.
impl<'de> Deserialize<'de> for ProviderTopology {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "kind", deny_unknown_fields)]
        enum Wire {
            #[serde(rename = "classic-v40")]
            ClassicV40 {},
            #[serde(rename = "grouped-v1")]
            GroupedV1 { contract_sha256: [u8; 32] },
            #[serde(rename = "ftrace-v1")]
            FtraceV1 { contract_sha256: [u8; 32] },
        }
        match Wire::deserialize(deserializer)? {
            Wire::ClassicV40 {} => Ok(Self::ClassicV40),
            Wire::GroupedV1 { contract_sha256 } => Ok(Self::GroupedV1 { contract_sha256 }),
            Wire::FtraceV1 { contract_sha256 } => Ok(Self::FtraceV1 { contract_sha256 }),
        }
    }
}

impl ProviderTopology {
    /// Reject incomplete metadata even when both ends supplied the same value.
    pub fn validate(&self) -> io::Result<()> {
        if matches!(self,
            Self::GroupedV1 { contract_sha256 } | Self::FtraceV1 { contract_sha256 }
                if *contract_sha256 == [0; 32])
        {
            return Err(io::Error::other("zero nonclassic provider contract digest"));
        }
        Ok(())
    }

    /// Declaration that the actual loaded driver's compile branch must report.
    pub fn driver_version(&self) -> u64 {
        match self {
            Self::ClassicV40 => 0,
            Self::GroupedV1 { .. } => 1,
            Self::FtraceV1 { .. } => 2,
        }
    }

    /// Join the actual loaded kind to already hash-bound expected metadata.
    /// The driver reports only its kind; it does not issue the contract digest
    /// or any grouped event ownership capability.
    pub(crate) fn observe_driver(&self, observed: u64) -> io::Result<Self> {
        self.validate()?;
        match (self, observed) {
            (Self::ClassicV40, 0) => Ok(Self::ClassicV40),
            (Self::GroupedV1 { contract_sha256 }, 1) => Ok(Self::GroupedV1 {
                contract_sha256: *contract_sha256,
            }),
            (Self::FtraceV1 { contract_sha256 }, 2) => Ok(Self::FtraceV1 {
                contract_sha256: *contract_sha256,
            }),
            _ => Err(io::Error::other("provider driver topology mismatch")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_topology_has_no_implicit_or_crossed_driver_kind() {
        let classic = ProviderTopology::ClassicV40;
        let grouped = ProviderTopology::GroupedV1 {
            contract_sha256: [7; 32],
        };
        let ftrace = ProviderTopology::FtraceV1 {
            contract_sha256: [8; 32],
        };
        assert_eq!(classic.observe_driver(0).unwrap(), classic);
        assert_eq!(grouped.observe_driver(1).unwrap(), grouped);
        assert_eq!(ftrace.observe_driver(2).unwrap(), ftrace);
        for value in [0, 1, 2, u64::MAX] {
            assert_eq!(classic.observe_driver(value).is_ok(), value == 0);
            assert_eq!(grouped.observe_driver(value).is_ok(), value == 1);
            assert_eq!(ftrace.observe_driver(value).is_ok(), value == 2);
        }
        assert!(
            ProviderTopology::GroupedV1 {
                contract_sha256: [0; 32]
            }
            .observe_driver(1)
            .is_err()
        );
        assert!(
            ProviderTopology::FtraceV1 {
                contract_sha256: [0; 32]
            }
            .observe_driver(2)
            .is_err()
        );
        for raw in [
            "null",
            "{}",
            r#"{"kind":"grouped-v1"}"#,
            r#"{"kind":"ftrace-v1"}"#,
            r#"{"kind":"classic-v40","contract_sha256":[]}"#,
            r#"{"kind":"grouped-v2"}"#,
        ] {
            assert!(serde_json::from_str::<ProviderTopology>(raw).is_err());
        }
    }

    #[test]
    fn provider_topology_strict_wire_round_trips_and_refuses_extra_or_duplicate_fields() {
        let classic = ProviderTopology::ClassicV40;
        let grouped = ProviderTopology::GroupedV1 {
            contract_sha256: [7; 32],
        };
        let ftrace = ProviderTopology::FtraceV1 {
            contract_sha256: [8; 32],
        };
        assert_eq!(
            serde_json::to_string(&classic).unwrap(),
            r#"{"kind":"classic-v40"}"#
        );
        for value in [classic, grouped, ftrace] {
            let raw = serde_json::to_string(&value).unwrap();
            assert_eq!(
                serde_json::from_str::<ProviderTopology>(&raw).unwrap(),
                value
            );
            for extra in [r#", "unknown":null}"#, r#", "unknown":[]}"#] {
                let changed = format!("{}{}", raw.strip_suffix('}').unwrap(), extra);
                assert!(
                    serde_json::from_str::<ProviderTopology>(&changed).is_err(),
                    "{changed}"
                );
            }
        }
        for raw in [
            r#"{"kind":"classic-v40","contract_sha256":[]}"#,
            r#"{"kind":"classic-v40","kind":"classic-v40"}"#,
            r#"{"kind":"classic-v40","kind":"grouped-v1"}"#,
            r#"{"kind":"classic-v40","kind":"ftrace-v1"}"#,
        ] {
            assert!(
                serde_json::from_str::<ProviderTopology>(raw).is_err(),
                "{raw}"
            );
        }
        let digest = serde_json::to_string(&[7u8; 32]).unwrap();
        let duplicate = format!(
            r#"{{"kind":"grouped-v1","contract_sha256":{digest},"contract_sha256":{digest}}}"#
        );
        assert!(serde_json::from_str::<ProviderTopology>(&duplicate).is_err());
    }
}
