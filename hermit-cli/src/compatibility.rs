/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Exact compatibility for a separately linked CLI backend executable.
//!
//! This is separate from the closed presentation/recording `BuildInfo`
//! schema. Equal ELF build IDs are neither expected nor used: the executables
//! differ, but must contain the same measured core source/API and build cohort.
//! Matching this record is not runtime qualification or publisher attestation.

#![allow(unexpected_cfgs, reason = "fbcode_build is supplied by Buck")]

use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;

/// Dedicated source/API contract; do not silently accept another version.
pub const SCHEMA: &str = "hermit-core-compatibility/v1";
/// The statically linked CLI application/adapter API.
pub const API: &str = "hermit-cli-static-backend/v1";
/// Independent framing for the identity-only executable probe.
pub const PROBE_SCHEMA: &str = "hermit-executable-probe/v1";
/// Both readers and producers enforce the same finite protocol output bound.
pub const MAX_PROBE_BYTES: usize = 16 * 1024;
/// This exact single option is handled before ordinary application startup.
pub const PROBE_ARG: &str = "--hermit-compatibility-v1";
/// Same identity response while an owned control socket keeps this executable
/// mapped. Only its parent owns the deadline/abandonment; it has no self timer.
pub const HELD_PROBE_ARG: &str = "--hermit-compatibility-held-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    kind: String,
    commit: String,
    tree: String,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReverieUnit {
    package: String,
    version: String,
    target: String,
    kind: Vec<String>,
    crate_types: Vec<String>,
    features: Vec<String>,
    profile: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reverie {
    commit: String,
    sha256: String,
    units: Vec<ReverieUnit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildOption {
    name: String,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RustcInputs {
    schema: String,
    normalization: String,
    shared_units: usize,
    shared_sha256: String,
}

/// Immutable complete comparison input measured by the core build producer.
/// All dimensions compare exactly; there is no tolerance or missing-field
/// interpretation, and extensions cannot replace the embedded core record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreCompatibility {
    schema: String,
    api: String,
    source: Source,
    release: String,
    reverie: Reverie,
    target: String,
    host: String,
    compiler: String,
    compiler_sha256: String,
    features: Vec<String>,
    core_profile: serde_json::Value,
    rustc_inputs: RustcInputs,
    configuration: Vec<BuildOption>,
}

fn full_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn full_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn known(value: &str) -> bool {
    !value.is_empty() && value != "unknown"
}

fn validate_profile(value: &serde_json::Value, owner: &str) -> Result<()> {
    let profile = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("{owner} has no Cargo artifact profile"))?;
    for key in [
        "opt_level",
        "debuginfo",
        "debug_assertions",
        "overflow_checks",
        "test",
    ] {
        ensure!(
            profile.contains_key(key),
            "{owner} omits Cargo profile option {key}"
        );
    }
    Ok(())
}

impl CoreCompatibility {
    /// Validate every required identity dimension before comparing records.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == SCHEMA && self.api == API,
            "unsupported core compatibility schema/API"
        );
        ensure!(
            self.source.kind == "git",
            "core compatibility has no measured Git source producer"
        );
        ensure!(
            full_oid(&self.source.commit) && full_oid(&self.source.tree),
            "core compatibility requires complete source commit and tree IDs"
        );
        ensure!(
            full_digest(&self.source.sha256),
            "core compatibility has no full source-content SHA256"
        );
        ensure!(
            known(&self.release),
            "core compatibility has no release identity"
        );
        ensure!(
            full_oid(&self.reverie.commit)
                && full_digest(&self.reverie.sha256)
                && !self.reverie.units.is_empty(),
            "core compatibility has no complete measured Reverie cohort"
        );
        let mut unit_keys = Vec::new();
        for unit in &self.reverie.units {
            ensure!(
                known(&unit.package)
                    && known(&unit.version)
                    && known(&unit.target)
                    && !unit.kind.is_empty()
                    && !unit.crate_types.is_empty(),
                "core compatibility has an incomplete actual Reverie unit"
            );
            ensure!(
                unit.features
                    .windows(2)
                    .all(|features| features[0] < features[1]),
                "core compatibility repeats or reorders Reverie unit features"
            );
            validate_profile(&unit.profile, "Reverie unit")?;
            unit_keys.push(serde_json::to_string(unit)?);
        }
        ensure!(
            unit_keys.windows(2).all(|keys| keys[0] < keys[1]),
            "core compatibility repeats or reorders actual Reverie units"
        );
        ensure!(
            known(&self.target) && known(&self.host),
            "core compatibility has no target/host identity"
        );
        ensure!(
            self.compiler
                .lines()
                .any(|line| line.strip_prefix("commit-hash: ").is_some_and(full_oid)),
            "core compatibility has no exact compiler commit"
        );
        ensure!(
            full_digest(&self.compiler_sha256),
            "core compatibility has no complete actual compiler frontend digest"
        );
        ensure!(
            self.features
                .iter()
                .all(|f| f.starts_with("CARGO_FEATURE_") && f.len() > "CARGO_FEATURE_".len()),
            "core compatibility has an invalid semantic feature"
        );
        ensure!(
            self.features.windows(2).all(|p| p[0] < p[1]),
            "core compatibility repeats or reorders semantic features"
        );
        validate_profile(&self.core_profile, "core library unit")?;
        ensure!(
            self.rustc_inputs.schema == "hermit-actual-rustc-capture/v1"
                && self.rustc_inputs.normalization == "hermit-shared-rustc-semantics/v1"
                && self.rustc_inputs.shared_units >= 2
                && full_digest(&self.rustc_inputs.shared_sha256),
            "core compatibility has no complete compared actual shared rustc inputs"
        );
        ensure!(
            self.configuration.windows(2).all(|p| p[0].name < p[1].name),
            "core compatibility repeats or reorders semantic build options"
        );
        for key in [
            "PROFILE",
            "OPT_LEVEL",
            "DEBUG",
            "CARGO_ENCODED_RUSTFLAGS",
            "RUSTFLAGS",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "CARGO_CFG_TARGET_ARCH",
            "CARGO_CFG_TARGET_OS",
            "CARGO_CFG_PANIC",
        ] {
            ensure!(
                self.configuration.iter().any(|option| option.name == key),
                "core compatibility omits build option {key}"
            );
        }
        for key in [
            "PROFILE",
            "OPT_LEVEL",
            "DEBUG",
            "CARGO_CFG_TARGET_ARCH",
            "CARGO_CFG_TARGET_OS",
            "CARGO_CFG_PANIC",
        ] {
            ensure!(
                self.configuration
                    .iter()
                    .any(|option| option.name == key && known(&option.value)),
                "core compatibility has no measured value for {key}"
            );
        }
        Ok(())
    }

    /// Refuse a mismatch in any complete dimension. A matching source commit
    /// alone cannot override feature, target, compiler or dependency drift.
    pub fn require_exact(&self, observed: &Self) -> Result<()> {
        self.validate()?;
        observed.validate()?;
        let dimension = if self.source != observed.source {
            "core source"
        } else if self.release != observed.release {
            "release"
        } else if self.reverie != observed.reverie {
            "Reverie cohort"
        } else if self.target != observed.target || self.host != observed.host {
            "target/host"
        } else if self.compiler != observed.compiler
            || self.compiler_sha256 != observed.compiler_sha256
        {
            "compiler"
        } else if self.features != observed.features {
            "semantic features"
        } else if self.core_profile != observed.core_profile {
            "core Cargo profile"
        } else if self.rustc_inputs != observed.rustc_inputs {
            "actual shared rustc semantics"
        } else if self.configuration != observed.configuration {
            "semantic build options"
        } else {
            return Ok(());
        };
        bail!(
            "backend executable is incompatible in {dimension}; rebuild it against this exact measured core cohort"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp() -> CoreCompatibility {
        let mut configuration: Vec<_> = [
            ("PROFILE", "release"),
            ("OPT_LEVEL", "3"),
            ("DEBUG", "false"),
            ("CARGO_ENCODED_RUSTFLAGS", ""),
            ("RUSTFLAGS", ""),
            ("RUSTC_WRAPPER", ""),
            ("RUSTC_WORKSPACE_WRAPPER", ""),
            ("CARGO_CFG_TARGET_ARCH", "x86_64"),
            ("CARGO_CFG_TARGET_OS", "linux"),
            ("CARGO_CFG_PANIC", "unwind"),
        ]
        .into_iter()
        .map(|(name, value)| BuildOption {
            name: name.to_owned(),
            value: value.to_owned(),
        })
        .collect();
        configuration.sort_by(|a, b| a.name.cmp(&b.name));
        CoreCompatibility {
            schema: SCHEMA.to_owned(),
            api: API.to_owned(),
            source: Source {
                kind: "git".to_owned(),
                commit: "1".repeat(40),
                tree: "2".repeat(40),
                sha256: "3".repeat(64),
            },
            release: "0.5.5".to_owned(),
            reverie: Reverie {
                commit: "4".repeat(40),
                sha256: "7".repeat(64),
                units: vec![ReverieUnit {
                    package: "reverie-core".to_owned(),
                    version: "0.5.5".to_owned(),
                    target: "reverie".to_owned(),
                    kind: vec!["lib".to_owned()],
                    crate_types: vec!["lib".to_owned()],
                    features: vec![],
                    profile: serde_json::json!({"opt_level":"3", "debuginfo":0,
                        "debug_assertions":false,"overflow_checks":false,"test":false}),
                }],
            },
            target: "x86_64-unknown-linux-gnu".to_owned(),
            host: "x86_64-unknown-linux-gnu".to_owned(),
            compiler: format!("rustc fixture\ncommit-hash: {}", "5".repeat(40)),
            compiler_sha256: "a".repeat(64),
            features: vec!["CARGO_FEATURE_LITEINST".to_owned()],
            core_profile: serde_json::json!({"opt_level":"3", "debuginfo":0,
                "debug_assertions":false,"overflow_checks":false,"test":false}),
            rustc_inputs: RustcInputs {
                schema: "hermit-actual-rustc-capture/v1".to_owned(),
                normalization: "hermit-shared-rustc-semantics/v1".to_owned(),
                shared_units: 2,
                shared_sha256: "8".repeat(64),
            },
            configuration,
        }
    }

    #[test]
    fn compatibility_requires_every_exact_dimension() {
        let original = stamp();
        original.require_exact(&original).unwrap();
        let changes: Vec<fn(&mut CoreCompatibility)> = vec![
            |s| s.source.commit = "6".repeat(40),
            |s| s.source.tree = "6".repeat(40),
            |s| s.source.sha256 = "6".repeat(64),
            |s| s.release = "0.5.6".to_owned(),
            |s| s.reverie.commit = "6".repeat(40),
            |s| s.reverie.sha256 = "6".repeat(64),
            |s| s.reverie.units[0].version = "0.5.6".to_owned(),
            |s| s.reverie.units[0].features.push("changed".to_owned()),
            |s| s.reverie.units[0].profile["overflow_checks"] = true.into(),
            |s| s.target = "x86_64-unknown-linux-musl".to_owned(),
            |s| s.host = "aarch64-unknown-linux-gnu".to_owned(),
            |s| s.compiler.push_str("\nother compiler input"),
            |s| s.compiler_sha256 = "b".repeat(64),
            |s| s.features.clear(),
            |s| s.core_profile["overflow_checks"] = true.into(),
            |s| s.rustc_inputs.shared_sha256 = "9".repeat(64),
            |s| s.configuration[0].value.push_str("other option"),
        ];
        for change in changes {
            let mut other = original.clone();
            change(&mut other);
            assert!(
                original.require_exact(&other).is_err(),
                "accepted {other:?}"
            );
        }
    }

    #[test]
    fn compatibility_refuses_incomplete_unknown_or_duplicate_dimensions() {
        let mut missing = stamp();
        missing.configuration.pop();
        assert!(missing.validate().is_err());
        let mut duplicate = stamp();
        duplicate
            .configuration
            .insert(0, duplicate.configuration[0].clone());
        assert!(duplicate.validate().is_err());
        let mut unknown = stamp();
        unknown.source.commit = "unknown".to_owned();
        assert!(unknown.validate().is_err());
        let mut value = serde_json::to_value(stamp()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("fallback".to_owned(), true.into());
        assert!(serde_json::from_value::<CoreCompatibility>(value).is_err());
        let mut missing = serde_json::to_value(stamp()).unwrap();
        missing.as_object_mut().unwrap().remove("core_profile");
        assert!(serde_json::from_value::<CoreCompatibility>(missing).is_err());
        let mut missing = serde_json::to_value(stamp()).unwrap();
        missing.as_object_mut().unwrap().remove("rustc_inputs");
        assert!(serde_json::from_value::<CoreCompatibility>(missing).is_err());
    }

    #[test]
    fn executable_probe_refuses_wrong_identity_trailing_data_and_unbounded_output() {
        let identity = ExecutableIdentity::backend("dynamorio").unwrap();
        let core = stamp();
        let probe = Probe {
            schema: PROBE_SCHEMA.to_owned(),
            executable: identity.clone(),
            core: core.clone(),
        };
        let bytes = probe.encode().unwrap();
        Probe::require(&bytes, &identity, &core).unwrap();
        assert!(Probe::require(&bytes, &ExecutableIdentity::Core, &core).is_err());
        let mut trailing = bytes.clone();
        trailing.extend_from_slice(b"{}\n");
        assert!(Probe::require(&trailing, &identity, &core).is_err());
        assert!(Probe::require(&vec![b' '; MAX_PROBE_BYTES + 1], &identity, &core).is_err());
    }
}

#[cfg(not(fbcode_build))]
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "availability", rename_all = "kebab-case", deny_unknown_fields)]
enum Embedded {
    Measured { stamp: Box<CoreCompatibility> },
    Unavailable { reason: String },
}

/// Return only a complete measured identity. Native/export/registry builds
/// without an explicit producer remain usable for builtins, but cannot claim
/// compatibility by substituting BuildInfo, a short revision, or a package name.
pub fn current() -> Result<CoreCompatibility> {
    #[cfg(fbcode_build)]
    bail!("extension compatibility unavailable: native/export producer is not implemented");

    #[cfg(not(fbcode_build))]
    {
        let bytes = option_env!("HERMIT_CORE_COMPATIBILITY").ok_or_else(|| {
            anyhow::anyhow!("extension compatibility unavailable: missing build producer")
        })?;
        ensure!(
            bytes.len() <= MAX_PROBE_BYTES,
            "embedded compatibility exceeds its protocol bound"
        );
        match serde_json::from_str::<Embedded>(bytes)? {
            Embedded::Measured { stamp } => {
                stamp.validate()?;
                Ok(*stamp)
            }
            Embedded::Unavailable { reason } => bail!(
                "extension compatibility unavailable: {reason}; use ci/build-compatible-cli.rs at the actual top-level build workspace with complete clean core source"
            ),
        }
    }
}

/// The executable's compiled selection, independent of any user option or
/// environment value. It does not authorize a private runtime role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ExecutableIdentity {
    /// The ordinary first-party CLI executable.
    Core,
    /// A separately linked executable with one static adapter.
    Backend { name: String },
}

impl ExecutableIdentity {
    /// Construct the identity that a static backend entry reports.
    pub fn backend(name: &str) -> Result<Self> {
        ensure!(
            !name.is_empty() && name.len() <= 64,
            "invalid executable backend identity"
        );
        ensure!(
            name.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "invalid executable backend identity"
        );
        ensure!(
            name.as_bytes()[0].is_ascii_lowercase() && !name.ends_with('-'),
            "invalid executable backend identity"
        );
        Ok(Self::Backend {
            name: name.to_owned(),
        })
    }
}

/// Closed identity-only probe response. This is not an execution receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    schema: String,
    executable: ExecutableIdentity,
    core: CoreCompatibility,
}

impl Probe {
    /// Construct from the compiled core and the entry's static adapter name.
    pub fn current(executable: ExecutableIdentity) -> Result<Self> {
        Ok(Self {
            schema: PROBE_SCHEMA.to_owned(),
            executable,
            core: current()?,
        })
    }

    /// Encode a finite response without entering the application.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.core.validate()?;
        let mut bytes = serde_json::to_vec(self)?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() <= MAX_PROBE_BYTES,
            "compatibility probe exceeds its output bound"
        );
        Ok(bytes)
    }

    /// Decode the complete response and bind both its exact backend identity
    /// and every core dimension. Trailing data and unknown fields are refused.
    pub fn require(
        bytes: &[u8],
        expected: &ExecutableIdentity,
        core: &CoreCompatibility,
    ) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_PROBE_BYTES,
            "compatibility probe exceeds its output bound"
        );
        let probe: Self = serde_json::from_slice(bytes)?;
        ensure!(
            probe.schema == PROBE_SCHEMA,
            "unsupported executable probe schema"
        );
        ensure!(
            &probe.executable == expected,
            "backend executable reported another identity; recursive/core redispatch is refused"
        );
        core.require_exact(&probe.core)?;
        Ok(probe)
    }
}
