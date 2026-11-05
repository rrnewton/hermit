//! Build provenance reported as the `build` object of `agentctl capabilities`.
//!
//! The source fingerprint is the value `AGENTCTL_SOURCE_FP` held when the binary was built
//! (see `build.rs`); it is null for a build that named none. The first line of `--version` is
//! not affected.

use serde_json::{json, Value};

/// Implementation edition of this binary.
pub(crate) const EDITION: &str = "rust";

/// Source fingerprint the binary was built from, when the build named one.
pub(crate) const SOURCE_FINGERPRINT: Option<&str> = option_env!("AGENTCTL_BUILD_SOURCE_FP");

/// The `build` object of the capabilities document: `{"edition", "source_fp"}`.
pub(crate) fn document() -> Value {
    json!({"edition": EDITION, "source_fp": SOURCE_FINGERPRINT})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_document_names_the_edition_and_a_checked_fingerprint() {
        let document = document();
        assert_eq!(document["edition"], "rust");
        let object = document.as_object().unwrap();
        assert_eq!(object.len(), 2);
        match &document["source_fp"] {
            Value::Null => assert!(SOURCE_FINGERPRINT.is_none()),
            Value::String(fingerprint) => {
                assert_eq!(fingerprint.len(), 64);
                assert!(fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
            }
            other => panic!("unexpected source_fp {other:?}"),
        }
    }
}
