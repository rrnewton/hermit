/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! What Hermit tells the in-guest LiteInst runtime at launch, carried in the
//! sealed preload bootstrap's tool data (`reverie_inguest::bootstrap`) rather
//! than in the guest's environment.
//!
//! The kernel copies a launch environment onto the guest's initial stack, so a
//! value there shifts every stack address and stays readable in
//! `/proc/self/environ` even after the runtime removes it. The bootstrap is a
//! sealed memfd the runtime reads and closes before the guest's code runs.

/// Identifies this record, and its version, at the start of the tool data.
const MAGIC: &[u8] = b"HERMIT-LITEINST-LAUNCH-V1\0";

const FORWARD: &str = "forward";
const POLICY: &str = "policy";
const FINGERPRINT: &str = "fingerprint";
const REQUIRE_LITEINST: &str = "require-liteinst";

/// Hermit's launch settings for one in-guest LiteInst run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InGuestLaunch {
    /// Require the absent-selector LiteInst default, without sealing a guest environment value.
    pub require_liteinst: bool,
    /// The DETLOG forwarding socket, as
    /// [`crate::detlog::tool_output_env_value`] writes it.
    pub forward_request: Option<String>,
    /// The forwarding policy, [`crate::detlog::ForwardPolicy::encode`].
    pub forward_policy: Option<String>,
    /// This build's [`crate::config_wire_fingerprint`].
    pub coordinator_fingerprint: Option<String>,
}

impl InGuestLaunch {
    /// The tool-data bytes: the magic, then `name=value` entries, each ended
    /// by a NUL byte. Values never contain NUL.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        for (name, value) in [
            (FORWARD, &self.forward_request),
            (POLICY, &self.forward_policy),
            (FINGERPRINT, &self.coordinator_fingerprint),
        ] {
            if let Some(value) = value {
                assert!(!value.contains('\0'), "{name} contains a NUL byte");
                bytes.extend_from_slice(name.as_bytes());
                bytes.push(b'=');
                bytes.extend_from_slice(value.as_bytes());
                bytes.push(0);
            }
        }
        if self.require_liteinst {
            bytes.extend_from_slice(b"require-liteinst=1\0");
        }
        bytes
    }

    /// Refuse an incompatible runtime before installing the Tool.
    pub fn validate_runtime_features(&self, liteinst_available: bool) -> Result<(), String> {
        if self.require_liteinst && !liteinst_available {
            return Err(
                "built without LiteInst; enable the `liteinst` Cargo feature to use site patching"
                    .to_owned(),
            );
        }
        Ok(())
    }

    /// Reads [`Self::encode`]'s bytes. Refuses another record, an unknown or
    /// repeated name, and an unterminated entry: a runtime that cannot read
    /// what Hermit sent must say so rather than run with less.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let entries = bytes
            .strip_prefix(MAGIC)
            .ok_or("the tool data is not a Hermit LiteInst launch record")?;
        let mut launch = Self::default();
        let mut rest = entries;
        while !rest.is_empty() {
            let end = rest
                .iter()
                .position(|byte| *byte == 0)
                .ok_or("a launch setting is not NUL-terminated")?;
            let entry = std::str::from_utf8(&rest[..end])
                .map_err(|_| "a launch setting is not UTF-8".to_owned())?;
            rest = &rest[end + 1..];
            let (name, value) = entry
                .split_once('=')
                .ok_or_else(|| format!("launch setting {entry:?} is not name=value"))?;
            if name == REQUIRE_LITEINST {
                if value != "1" || launch.require_liteinst {
                    return Err("unreadable or repeated LiteInst runtime requirement".to_owned());
                }
                launch.require_liteinst = true;
                continue;
            }
            let slot = match name {
                FORWARD => &mut launch.forward_request,
                POLICY => &mut launch.forward_policy,
                FINGERPRINT => &mut launch.coordinator_fingerprint,
                _ => return Err(format!("unknown launch setting {name:?}")),
            };
            if slot.replace(value.to_owned()).is_some() {
                return Err(format!("launch setting {name:?} is repeated"));
            }
        }
        Ok(launch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_record_round_trips() {
        for launch in [
            InGuestLaunch::default(),
            InGuestLaunch {
                require_liteinst: true,
                forward_request: Some("7:12345:67890".to_owned()),
                forward_policy: Some("info,detcore::random=warn".to_owned()),
                coordinator_fingerprint: Some("9349c85b2c48fc92".to_owned()),
            },
            InGuestLaunch {
                coordinator_fingerprint: Some("0123456789abcdef".to_owned()),
                ..Default::default()
            },
        ] {
            assert_eq!(InGuestLaunch::decode(&launch.encode()), Ok(launch));
        }
    }

    #[test]
    fn a_launch_record_refuses_what_it_cannot_read() {
        let mut repeated = MAGIC.to_vec();
        repeated.extend_from_slice(b"policy=a\0policy=b\0");
        let mut unknown = MAGIC.to_vec();
        unknown.extend_from_slice(b"colour=blue\0");
        let mut unterminated = MAGIC.to_vec();
        unterminated.extend_from_slice(b"policy=a");
        let mut invalid_requirement = MAGIC.to_vec();
        invalid_requirement.extend_from_slice(b"require-liteinst=0\0");
        let mut repeated_requirement = MAGIC.to_vec();
        repeated_requirement.extend_from_slice(b"require-liteinst=1\0require-liteinst=1\0");
        for bytes in [
            &b"REVERIE-LI-V1"[..],
            &repeated,
            &unknown,
            &unterminated,
            &invalid_requirement,
            &repeated_requirement,
        ] {
            assert!(InGuestLaunch::decode(bytes).is_err(), "{bytes:?}");
        }
    }
}

#[cfg(test)]
mod runtime_feature_requirement_tests {
    use super::InGuestLaunch;

    #[test]
    fn liteinst_intent_refuses_a_trap_only_runtime_independently_of_environment() {
        let required = InGuestLaunch {
            require_liteinst: true,
            ..Default::default()
        };
        assert!(required.validate_runtime_features(false).is_err());
        assert!(required.validate_runtime_features(true).is_ok());
        assert!(
            InGuestLaunch::default()
                .validate_runtime_features(false)
                .is_ok()
        );
        assert!(
            InGuestLaunch::default()
                .validate_runtime_features(true)
                .is_ok()
        );
    }
}
