use std::path::PathBuf;

use serde_json::Value;

pub(crate) fn selected_staticlib(messages: &str, package: &str) -> Result<PathBuf, String> {
    let mut paths = Vec::new();
    for line in messages.lines().filter(|line| !line.is_empty()) {
        let message: Value = serde_json::from_str(line).map_err(|error| error.to_string())?;
        if message["reason"] != "compiler-artifact"
            || message["package_id"] != package
            || message["target"]["name"] != "hermit_liteinst_detcore"
        {
            continue;
        }
        if message["profile"]["test"] != false
            || message["features"] != serde_json::json!(["private-crt"])
        {
            return Err("unexpected private archive profile/features".to_owned());
        }
        for filename in message["filenames"]
            .as_array()
            .ok_or("missing archive filenames")?
        {
            let path = PathBuf::from(filename.as_str().ok_or("invalid archive filename")?);
            if path.extension().is_some_and(|extension| extension == "a") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths.dedup();
    if paths.len() != 1 {
        return Err(format!(
            "expected one current private archive, got {}",
            paths.len()
        ));
    }
    Ok(paths.remove(0))
}

pub(crate) fn liteinst_cdylibs_from_cargo_messages(
    messages: &str,
    package: &str,
) -> Result<Vec<PathBuf>, String> {
    let mut artifacts = Vec::new();
    for (index, line) in messages.lines().filter(|line| !line.is_empty()).enumerate() {
        let message: Value = serde_json::from_str(line)
            .map_err(|error| format!("invalid Cargo JSON on line {}: {error}", index + 1))?;
        let target = &message["target"];
        let is_liteinst_cdylib = message["reason"] == "compiler-artifact"
            && message["package_id"] == package
            && target["name"] == "hermit_liteinst_detcore"
            && target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "cdylib"));
        if !is_liteinst_cdylib {
            continue;
        }
        let filenames = message["filenames"].as_array().ok_or_else(|| {
            format!(
                "LiteInst compiler-artifact on line {} has no filenames",
                index + 1
            )
        })?;
        for filename in filenames {
            let path = PathBuf::from(
                filename
                    .as_str()
                    .ok_or("non-string Cargo artifact filename")?,
            );
            if path.extension().is_some_and(|extension| extension == "so") {
                artifacts.push(path);
            }
        }
    }
    artifacts.sort();
    artifacts.dedup();
    Ok(artifacts)
}

pub(crate) fn selected_cdylib(messages: &str, package: &str) -> Result<PathBuf, String> {
    let artifacts = liteinst_cdylibs_from_cargo_messages(messages, package)?;
    if artifacts.len() != 1 {
        return Err(format!(
            "expected one current Detcore cdylib, got {}",
            artifacts.len()
        ));
    }
    Ok(artifacts.into_iter().next().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_current_private_archive_and_features() {
        let message = serde_json::json!({"reason":"compiler-artifact", "package_id":"selected", "target":{"name":"hermit_liteinst_detcore","kind":["cdylib"]}, "features":["private-crt"], "profile":{"test":false}, "filenames":["current.a"]});
        assert_eq!(
            selected_staticlib(&message.to_string(), "selected").unwrap(),
            PathBuf::from("current.a")
        );
        assert!(selected_staticlib(&message.to_string(), "foreign").is_err());
        for (field, value) in [
            ("features", serde_json::json!([])),
            ("filenames", serde_json::json!(["one.a", "two.a"])),
            ("profile", serde_json::json!({"test":true})),
        ] {
            let mut bad = message.clone();
            bad[field] = value;
            assert!(selected_staticlib(&bad.to_string(), "selected").is_err());
        }
        assert!(selected_staticlib("not-json", "selected").is_err());
    }

    #[test]
    fn selects_only_the_current_liteinst_cdylib_message() {
        let messages = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"unrelated","kind":["cdylib"]},"filenames":["/warm/deps/libreverie_liteinst-stale.so"]}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"selected-package","target":{"name":"hermit_liteinst_detcore","kind":["rlib","cdylib"]},"filenames":["/isolated/deps/current.rlib","/isolated/deps/current.so"],"fresh":true}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
        );

        assert_eq!(
            liteinst_cdylibs_from_cargo_messages(messages, "selected-package").unwrap(),
            [PathBuf::from("/isolated/deps/current.so")]
        );
    }

    #[test]
    fn rejects_non_json_output() {
        let error =
            liteinst_cdylibs_from_cargo_messages("not cargo json", "selected-package").unwrap_err();
        assert!(error.contains("invalid Cargo JSON on line 1"));
    }

    #[test]
    fn rejects_missing_multiple_foreign_and_malformed_candidates() {
        assert!(selected_cdylib("", "selected").is_err());
        let mut message = serde_json::json!({"reason":"compiler-artifact", "package_id":"selected",
            "target":{"name":"hermit_liteinst_detcore", "kind":["cdylib"]}, "filenames":["one.so"]});
        let valid = message.to_string();
        assert_eq!(
            selected_cdylib(&format!("{valid}\n{valid}"), "selected").unwrap(),
            PathBuf::from("one.so")
        );
        message["filenames"] = serde_json::json!(["one.so", "two.so"]);
        assert!(selected_cdylib(&message.to_string(), "selected").is_err());
        message["filenames"] = serde_json::json!(["one.so", 9]);
        assert!(selected_cdylib(&message.to_string(), "selected").is_err());
        assert!(selected_cdylib(&valid, "foreign").is_err());
        message["target"]["name"] = serde_json::json!("reverie_liteinst");
        assert!(selected_cdylib(&message.to_string(), "selected").is_err());
    }
}
