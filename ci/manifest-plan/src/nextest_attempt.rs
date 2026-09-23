//! Coverage for the maintained wrapper's actual nextest attempt parser.
//! Included only by its test build; no second production parser exists.

use std::ffi::OsString;

pub const PUBLIC_ENV: &str = super::ATTEMPT_ENV;
pub const LEGACY_ENV: &str = super::LEGACY_ATTEMPT_ENV;

fn from_values(public: Option<OsString>, legacy: Option<OsString>) -> Result<u64, String> {
    fn text(name: &str, value: Option<OsString>) -> Result<Option<String>, String> {
        value
            .map(|value| {
                value
                    .into_string()
                    .map_err(|_| format!("{name} must be valid UTF-8"))
            })
            .transpose()
    }
    let public = text(PUBLIC_ENV, public)?;
    let legacy = text(LEGACY_ENV, legacy)?;
    super::attempt_from_values(public.as_deref(), legacy.as_deref())
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    #[test]
    fn public_attempt_is_used_without_a_legacy_value() {
        assert_eq!(from_values(Some("7".into()), None), Ok(7));
    }

    #[test]
    fn legacy_attempt_is_supported_when_public_is_absent() {
        assert_eq!(from_values(None, Some("3".into())), Ok(3));
    }

    #[test]
    fn equal_dual_attempts_identify_the_same_execution() {
        assert_eq!(from_values(Some("5".into()), Some("5".into())), Ok(5));
    }

    #[test]
    fn conflicting_dual_attempts_are_rejected() {
        let error = from_values(Some("5".into()), Some("6".into())).unwrap_err();
        assert!(error.contains("disagree"));
        assert!(error.contains(PUBLIC_ENV) && error.contains(LEGACY_ENV));
    }

    #[test]
    fn numerically_equal_but_differently_spelled_dual_attempts_are_rejected() {
        assert!(from_values(Some("01".into()), Some("1".into())).is_err());
        assert!(from_values(Some("1".into()), Some("01".into())).is_err());
    }

    #[test]
    fn absent_attempt_is_not_replaced_with_one() {
        assert!(from_values(None, None).is_err());
    }

    #[test]
    fn invalid_present_values_never_fall_back_to_the_other_authority() {
        for raw in ["", "0", "-1", " 1", "1 ", "one", "18446744073709551616"] {
            assert!(
                from_values(Some(raw.into()), None).is_err(),
                "public {raw:?}"
            );
            assert!(
                from_values(None, Some(raw.into())).is_err(),
                "legacy {raw:?}"
            );
            assert!(from_values(Some(raw.into()), Some("1".into())).is_err());
            assert!(from_values(Some("1".into()), Some(raw.into())).is_err());
        }
    }

    #[test]
    fn non_utf8_attempts_are_rejected_even_with_a_valid_other_authority() {
        let raw = OsString::from_vec(vec![0xff]);
        assert!(from_values(Some(raw.clone()), None).is_err());
        assert!(from_values(None, Some(raw.clone())).is_err());
        assert!(from_values(Some(raw.clone()), Some("1".into())).is_err());
        assert!(from_values(Some("1".into()), Some(raw)).is_err());
    }

    #[test]
    fn maximum_attempt_is_preserved_without_truncation() {
        assert_eq!(
            from_values(Some(u64::MAX.to_string().into()), None),
            Ok(u64::MAX)
        );
    }
}
