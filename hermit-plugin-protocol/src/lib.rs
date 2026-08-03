//! Dependency-light, fail-closed protocol between Hermit and optional backend helpers.

#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// Protocol major understood by this host and helper.
pub const PROTOCOL_VERSION: u32 = 1;
/// Exact Detcore native callback ABI implemented by the DBT plugin.
pub const DETCORE_ABI_TAG: &str = "hdt1";
/// Exact Detcore/SaBRe shared-object ABI implemented by the SaBRe plugin.
pub const SABRE_DETCORE_ABI_TAG: &str = "hsb1";
/// Exact host/tool handoff understood by the e9patch package.
pub const E9PATCH_ABI_TAG: &str = "hep1";
/// Exported ELF data symbol containing a [`DetcoreDescriptorV1`].
pub const DETCORE_DESCRIPTOR_SYMBOL: &str = "HERMIT_DETCORE_PLUGIN_DESCRIPTOR_V1";
/// `sysexits.h` unavailable-service status used for an absent helper.
pub const EX_UNAVAILABLE: i32 = 69;
/// `sysexits.h` creation failure used for an unwritable Hermit root.
pub const EX_CANTCREAT: i32 = 73;
/// `sysexits.h` configuration failure used for incompatible components.
pub const EX_CONFIG: i32 = 78;

/// Fixed C layout exported by the Detcore DBT shared object.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct DetcoreDescriptorV1 {
    /// Size of this descriptor, allowing fail-closed extension checks.
    pub size: u32,
    /// Control protocol major.
    pub protocol: u32,
    /// NUL-terminated Detcore ABI tag.
    pub detcore_abi: [u8; 16],
    /// NUL-terminated lowercase hexadecimal Detcore build ID.
    pub detcore_build_id: [u8; 65],
}

impl DetcoreDescriptorV1 {
    /// Constructs the exported descriptor, rejecting values that do not fit the ABI.
    pub const fn new(detcore_build_id: &str) -> Self {
        Self::with_abi(DETCORE_ABI_TAG, detcore_build_id)
    }

    /// Constructs a descriptor for a specific backend ABI.
    pub const fn with_abi(detcore_abi: &str, detcore_build_id: &str) -> Self {
        assert!(detcore_abi.len() < 16, "Detcore ABI tag is too long");
        assert!(
            detcore_build_id.len() == 64,
            "Detcore build ID must be a SHA-256 hex digest"
        );
        let mut descriptor = Self {
            size: std::mem::size_of::<Self>() as u32,
            protocol: PROTOCOL_VERSION,
            detcore_abi: [0; 16],
            detcore_build_id: [0; 65],
        };
        let abi = detcore_abi.as_bytes();
        let mut index = 0;
        while index < abi.len() {
            descriptor.detcore_abi[index] = abi[index];
            index += 1;
        }
        let build = detcore_build_id.as_bytes();
        index = 0;
        while index < build.len() {
            descriptor.detcore_build_id[index] = build[index];
            index += 1;
        }
        descriptor
    }

    /// Returns the ABI tag after validating NUL termination and UTF-8.
    pub fn abi_tag(&self) -> Option<&str> {
        fixed_c_string(&self.detcore_abi)
    }

    /// Returns the build ID after validating NUL termination and UTF-8.
    pub fn build_id(&self) -> Option<&str> {
        fixed_c_string(&self.detcore_build_id)
    }
}

fn fixed_c_string(bytes: &[u8]) -> Option<&str> {
    let end = bytes.iter().position(|byte| *byte == 0)?;
    std::str::from_utf8(&bytes[..end]).ok()
}

/// Exact identity that must agree before a backend payload can run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PluginIdentity {
    /// Backend selected by the host; a helper for another backend is incompatible.
    pub backend: String,
    /// Control protocol major.
    pub protocol: u32,
    /// Exact Hermit Cargo package version.
    pub package_version: String,
    /// Supported operating-system and architecture pair.
    pub target: String,
    /// Exact Detcore native callback ABI tag.
    pub detcore_abi: String,
    /// Exact Detcore source/build-input identity.
    pub detcore_build_id: String,
}

impl PluginIdentity {
    /// Constructs a DBT identity for the current target.
    pub fn current(backend: &str, package_version: &str, detcore_build_id: &str) -> Self {
        Self::with_abi(backend, package_version, DETCORE_ABI_TAG, detcore_build_id)
    }

    /// Constructs an identity for a specific backend ABI and current target.
    pub fn with_abi(
        backend: &str,
        package_version: &str,
        detcore_abi: &str,
        detcore_build_id: &str,
    ) -> Self {
        Self {
            backend: backend.to_owned(),
            protocol: PROTOCOL_VERSION,
            package_version: package_version.to_owned(),
            target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            detcore_abi: detcore_abi.to_owned(),
            detcore_build_id: detcore_build_id.to_owned(),
        }
    }

    /// Returns the first exact-identity mismatch.
    pub fn mismatch(&self, plugin: &Self) -> Option<&'static str> {
        if self.backend != plugin.backend {
            Some("backend")
        } else if self.protocol != plugin.protocol {
            Some("protocol")
        } else if self.package_version != plugin.package_version {
            Some("package version")
        } else if self.target != plugin.target {
            Some("target")
        } else if self.detcore_abi != plugin.detcore_abi {
            Some("Detcore ABI")
        } else if self.detcore_build_id != plugin.detcore_build_id {
            Some("Detcore build ID")
        } else {
            None
        }
    }

    /// Stable content-addressed directory component for this identity.
    pub fn release_key(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.package_version, self.target, self.detcore_abi, self.detcore_build_id
        )
    }
}

/// Request sent by the host to the selected helper on standard input.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EnsureRequest {
    /// Exact host identity the helper must satisfy.
    pub host: PluginIdentity,
}

/// Validated payload paths returned by the helper on standard output.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PayloadManifest {
    /// Exact identity embedded in the helper and payload.
    pub plugin: PluginIdentity,
    /// Content-addressed extracted release directory.
    pub release_dir: PathBuf,
    /// DynamoRIO launcher.
    pub drrun: PathBuf,
    /// Native DynamoRIO client.
    pub client: PathBuf,
    /// Detcore DBT shared object.
    pub detcore_runtime: PathBuf,
    /// SaBRe loader, empty for other backends.
    #[serde(default)]
    pub sabre: PathBuf,
    /// e9tool executable, empty for other backends.
    #[serde(default)]
    pub e9tool: PathBuf,
    /// e9patch backend executable, empty for other backends.
    #[serde(default)]
    pub e9patch: PathBuf,
    /// SHA-256 by payload-relative path.
    pub files: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_identity_rejects_every_skew_dimension() {
        let host = PluginIdentity::current("dbt", "0.2.0", "build-a");
        assert_eq!(host.mismatch(&host), None);

        let mut plugin = host.clone();
        plugin.backend = "sabre".to_owned();
        assert_eq!(host.mismatch(&plugin), Some("backend"));
        plugin = host.clone();
        plugin.package_version = "0.2.1".to_owned();
        assert_eq!(host.mismatch(&plugin), Some("package version"));
        plugin = host.clone();
        plugin.detcore_abi = "hdt2".to_owned();
        assert_eq!(host.mismatch(&plugin), Some("Detcore ABI"));
        plugin = host.clone();
        plugin.detcore_build_id = "build-b".to_owned();
        assert_eq!(host.mismatch(&plugin), Some("Detcore build ID"));
    }

    #[test]
    fn native_descriptor_is_fixed_and_nul_terminated() {
        let build_id = "a".repeat(64);
        let descriptor = DetcoreDescriptorV1::new(&build_id);
        assert_eq!(descriptor.size as usize, std::mem::size_of_val(&descriptor));
        assert_eq!(descriptor.protocol, PROTOCOL_VERSION);
        assert_eq!(descriptor.abi_tag(), Some(DETCORE_ABI_TAG));
        assert_eq!(descriptor.build_id(), Some(build_id.as_str()));
    }
}
