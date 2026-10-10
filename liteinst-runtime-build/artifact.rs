use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

pub(crate) fn liteinst_cdylib_from_cargo_messages(
    messages: &str,
    package_id: &str,
    manifest: &Path,
) -> Result<PathBuf, String> {
    let mut artifacts = Vec::new();
    for (index, line) in messages.lines().filter(|line| !line.is_empty()).enumerate() {
        let message: Value = serde_json::from_str(line)
            .map_err(|error| format!("invalid Cargo JSON on line {}: {error}", index + 1))?;
        let target = &message["target"];
        let is_preload = message["reason"] == "compiler-artifact"
            && target["name"] == "reverie_liteinst_preload";
        if !is_preload {
            continue;
        }
        if message["package_id"] != package_id
            || message["manifest_path"].as_str().map(Path::new) != Some(manifest)
            || target["kind"] != serde_json::json!(["cdylib"])
            || target["crate_types"] != serde_json::json!(["cdylib"])
        {
            return Err(format!(
                "LiteInst preload artifact on line {} has the wrong package, manifest or cdylib-only target",
                index + 1
            ));
        }
        if message["features"] != serde_json::json!([]) {
            return Err(format!(
                "LiteInst preload artifact on line {} has unexpected features",
                index + 1
            ));
        }
        let filenames = message["filenames"].as_array().ok_or_else(|| {
            format!(
                "LiteInst compiler-artifact on line {} has no filenames",
                index + 1
            )
        })?;
        if filenames.len() != 1 {
            return Err("LiteInst preload must report exactly one current cdylib output".into());
        }
        let filename = filenames[0]
            .as_str()
            .ok_or("LiteInst preload filename is not a string")?;
        let path = PathBuf::from(filename);
        if path.file_name().and_then(|name| name.to_str()) != Some("libreverie_liteinst_preload.so")
        {
            return Err("LiteInst preload output does not have the leaf's exact filename".into());
        }
        artifacts.push(path);
    }
    if artifacts.len() != 1 {
        return Err(format!(
            "expected exactly one LiteInst preload cdylib in current Cargo output, found {artifacts:?}"
        ));
    }
    Ok(artifacts.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKAGE: &str = "git+https://github.com/rrnewton/reverie.git?rev=0123456789012345678901234567890123456789#reverie-liteinst-preload@0.4.1";
    const MANIFEST: &str = "/git/reverie/reverie-liteinst-preload/Cargo.toml";

    fn artifact() -> Value {
        serde_json::json!({
            "reason": "compiler-artifact", "package_id": PACKAGE,
            "manifest_path": MANIFEST,
            "target": {"name":"reverie_liteinst_preload", "kind":["cdylib"], "crate_types":["cdylib"]},
            "features": [],
            "filenames": ["/isolated/libreverie_liteinst_preload.so"], "fresh":true
        })
    }

    fn select(messages: &str) -> Result<PathBuf, String> {
        liteinst_cdylib_from_cargo_messages(messages, PACKAGE, Path::new(MANIFEST))
    }

    #[test]
    fn selects_only_the_current_liteinst_cdylib_message() {
        let messages = format!(
            "{}\n{}\n{}\n{}\n",
            serde_json::json!({"reason":"compiler-artifact","target":{"name":"unrelated","kind":["cdylib"]},"filenames":["/warm/libreverie_liteinst-stale.so"]}),
            serde_json::json!({"reason":"compiler-artifact","target":{"name":"reverie_liteinst","kind":["rlib"]},"filenames":["/isolated/libreverie_liteinst.rlib"]}),
            artifact(),
            serde_json::json!({"reason":"build-finished","success":true}),
        );

        assert_eq!(
            select(&messages).unwrap(),
            PathBuf::from("/isolated/libreverie_liteinst_preload.so")
        );
    }

    #[test]
    fn rejects_non_json_output() {
        let error = select("not cargo json").unwrap_err();
        assert!(error.contains("invalid Cargo JSON on line 1"));
    }

    #[test]
    fn refuses_missing_or_stale_core_artifacts() {
        assert!(select("").is_err());
        let stale = serde_json::json!({
            "reason":"compiler-artifact",
            "target":{"name":"reverie_liteinst","kind":["rlib","cdylib"]},
            "filenames":["/warm/libreverie_liteinst.so"]
        });
        assert!(select(&stale.to_string()).is_err());
    }

    #[test]
    fn refuses_duplicate_current_artifact_messages() {
        let message = artifact();
        assert!(select(&format!("{message}\n{message}\n")).is_err());
    }

    #[test]
    fn refuses_wrong_source_target_features_or_filename() {
        for (field, value) in [
            (
                "package_id",
                serde_json::json!(
                    "git+https://github.com/rrnewton/reverie.git?rev=1111111111111111111111111111111111111111#reverie-liteinst-preload@0.4.1"
                ),
            ),
            ("manifest_path", serde_json::json!("/lookalike/Cargo.toml")),
            ("features", serde_json::json!(["allocator-fixture"])),
            (
                "filenames",
                serde_json::json!(["/warm/libreverie_liteinst.so"]),
            ),
        ] {
            let mut message = artifact();
            message[field] = value;
            assert!(
                select(&message.to_string()).is_err(),
                "accepted wrong {field}"
            );
        }
        for field in ["kind", "crate_types"] {
            let mut message = artifact();
            message["target"][field] = serde_json::json!(["rlib", "cdylib"]);
            assert!(
                select(&message.to_string()).is_err(),
                "accepted wrong {field}"
            );
        }
    }

    #[test]
    fn refuses_missing_malformed_or_multiple_filenames() {
        for value in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!([42]),
            serde_json::json!([
                "/isolated/libreverie_liteinst_preload.so",
                "/warm/libreverie_liteinst_preload.so"
            ]),
        ] {
            let mut message = artifact();
            message["filenames"] = value;
            assert!(select(&message.to_string()).is_err());
        }
    }
}
