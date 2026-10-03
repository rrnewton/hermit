//! Native provider grammar metadata. This is not a prepared-Call capability.

use std::io;

use serde::Deserialize;
use serde::Serialize;

/// The only supported adapter/receive-copy grammar pairs. A package cannot
/// independently choose an adapter ABI and reinterpret its observation bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderWireFormat {
    /// Historical adapter ABI7 and the unchanged receive-copy version4.
    #[serde(rename = "abi7-copy4")]
    Abi7Copy4,
    /// Adapter ABI8 with explicit receive attempts and byte-frontier version5.
    #[serde(rename = "abi8-copy5")]
    Abi8Copy5,
    /// Adapter ABI9 carries the explicit112-byte descriptor dispatch event.
    #[serde(rename = "abi9-copy4")]
    Abi9Copy4,
    /// Same descriptor ABI with explicit receive attempts/frontier version5.
    #[serde(rename = "abi9-copy5")]
    Abi9Copy5,
}

impl ProviderWireFormat {
    /// Validate an explicit pair; neither unknown versions nor crossed pairs
    /// permit fallback to a different grammar.
    pub fn from_versions(abi: u64, copy: u64) -> io::Result<Self> {
        match (abi, copy) {
            (0x4150_5255_5354_0007, 4) => Ok(Self::Abi7Copy4),
            (0x4150_5255_5354_0008, 5) => Ok(Self::Abi8Copy5),
            (0x4150_5255_5354_0009, 4) => Ok(Self::Abi9Copy4),
            (0x4150_5255_5354_0009, 5) => Ok(Self::Abi9Copy5),
            _ => Err(io::Error::other(
                "unsupported provider adapter/copy version pair",
            )),
        }
    }

    /// Parse the authenticated package declaration. Historical ABI7 packages
    /// predate the copy field and unambiguously specify version4. ABI8 requires
    /// an explicit version5 field; raw frames never choose either version.
    pub fn from_package(abi: &str, copy: Option<u64>) -> io::Result<Self> {
        match (abi, copy) {
            ("4150525553540007", None | Some(4)) => Ok(Self::Abi7Copy4),
            ("4150525553540008", Some(5)) => Ok(Self::Abi8Copy5),
            ("4150525553540009", Some(4)) => Ok(Self::Abi9Copy4),
            ("4150525553540009", Some(5)) => Ok(Self::Abi9Copy5),
            _ => Err(io::Error::other(
                "unsupported provider package wire declaration",
            )),
        }
    }

    /// Exact adapter ABI that the authenticated DSO must report.
    pub fn abi_version(self) -> u64 {
        match self {
            Self::Abi7Copy4 => 0x4150_5255_5354_0007,
            Self::Abi8Copy5 => 0x4150_5255_5354_0008,
            Self::Abi9Copy4 | Self::Abi9Copy5 => 0x4150_5255_5354_0009,
        }
    }

    /// Descriptor-event layout is selected only by the authenticated adapter.
    pub(crate) fn has_source_ioctl_dispatch(self) -> bool {
        matches!(self, Self::Abi9Copy4 | Self::Abi9Copy5)
    }

    /// Receive-copy grammar bound before any observation is parsed.
    pub fn copy_version(self) -> u64 {
        match self {
            Self::Abi7Copy4 | Self::Abi9Copy4 => 4,
            Self::Abi8Copy5 | Self::Abi9Copy5 => 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_copy_wire_pairs_are_exact_and_never_frame_selected() {
        for pair in [ProviderWireFormat::Abi7Copy4, ProviderWireFormat::Abi8Copy5] {
            assert_eq!(
                ProviderWireFormat::from_versions(pair.abi_version(), pair.copy_version()).unwrap(),
                pair
            );
            for other in [0, 1, 3, 4, 5, 6, u64::MAX] {
                if other != pair.copy_version() {
                    assert!(ProviderWireFormat::from_versions(pair.abi_version(), other).is_err());
                }
            }
        }
        for abi in [
            0,
            7,
            8,
            0x4150_5255_5354_0006,
            0x4150_5255_5354_000a,
            u64::MAX,
        ] {
            for copy in [4, 5] {
                assert!(ProviderWireFormat::from_versions(abi, copy).is_err());
            }
        }
    }

    #[test]
    fn dispatch_event_abi_requires_explicit_grammar_and_distinct_legacy_layout() {
        for (copy, expected) in [
            (4, ProviderWireFormat::Abi9Copy4),
            (5, ProviderWireFormat::Abi9Copy5),
        ] {
            assert_eq!(
                ProviderWireFormat::from_versions(0x4150_5255_5354_0009, copy).unwrap(),
                expected
            );
            assert_eq!(
                ProviderWireFormat::from_package("4150525553540009", Some(copy)).unwrap(),
                expected
            );
            assert!(expected.has_source_ioctl_dispatch());
            assert_eq!(expected.copy_version(), copy);
            assert_eq!(expected.abi_version(), 0x4150_5255_5354_0009);
        }
        for copy in [None, Some(0), Some(3), Some(6), Some(u64::MAX)] {
            assert!(ProviderWireFormat::from_package("4150525553540009", copy).is_err());
        }
        assert!(!ProviderWireFormat::Abi7Copy4.has_source_ioctl_dispatch());
        assert!(!ProviderWireFormat::Abi8Copy5.has_source_ioctl_dispatch());
    }

    #[test]
    fn old_package_declaration_is_v4_and_v5_requires_explicit_pair() {
        assert_eq!(
            ProviderWireFormat::from_package("4150525553540007", None).unwrap(),
            ProviderWireFormat::Abi7Copy4
        );
        assert_eq!(
            ProviderWireFormat::from_package("4150525553540007", Some(4)).unwrap(),
            ProviderWireFormat::Abi7Copy4
        );
        assert_eq!(
            ProviderWireFormat::from_package("4150525553540008", Some(5)).unwrap(),
            ProviderWireFormat::Abi8Copy5
        );
        for (abi, copy) in [
            ("4150525553540008", None),
            ("4150525553540008", Some(4)),
            ("4150525553540007", Some(5)),
            ("4150525553540006", Some(4)),
            ("0x4150525553540007", Some(4)),
            ("4150525553540007\n", Some(4)),
        ] {
            assert!(ProviderWireFormat::from_package(abi, copy).is_err());
        }
        assert!(serde_json::from_str::<ProviderWireFormat>("\"abi8-copy4\"").is_err());
    }
}
