//! Finalize the ordinary packaged Detcore preload after the outer Cargo build.
//! This executable must never be called by a Cargo build script.
use std::fs;
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use reverie_liteinst_artifact as proof;
use serde_json::Value;
use serde_json::json;

include!("../../../hermit-cli/reverie_pin.rs");

fn require(ok: bool, message: &str) -> proof::Result<()> {
    if ok { Ok(()) } else { Err(message.into()) }
}

/// The one name the ordinary Detcore Tool image is published under.
const CANONICAL_TOOL_IMAGE: &str = "libdetcore_liteinst.so";

/// Derive the staged Tool image's site-patching state from the current
/// Cargo build's own artifact metadata: every `reverie-liteinst` rlib it
/// compiled must carry the `liteinst` feature. A trap-only (patcher-less)
/// image is refused by its canonical name, never published under it and
/// never re-labelled by a receipt field alone; the returned label is the
/// explicit `site_patching` qualification recorded in the packaged receipt.
fn site_patching_from_cargo_messages(messages: &str) -> proof::Result<&'static str> {
    let mut saw_rlib = false;
    for (index, line) in messages.lines().filter(|line| !line.is_empty()).enumerate() {
        let message: Value = serde_json::from_str(line)
            .map_err(|error| format!("invalid Cargo JSON on line {}: {error}", index + 1))?;
        let target = &message["target"];
        if message["reason"] != "compiler-artifact" || target["name"] != "reverie_liteinst" {
            continue;
        }
        if target["kind"] != json!(["rlib"]) || target["crate_types"] != json!(["rlib"]) {
            return Err(format!(
                "refusing to stage the Detcore Tool image as {CANONICAL_TOOL_IMAGE}: \
                 reverie-liteinst artifact on line {} has the wrong rlib target",
                index + 1
            ));
        }
        let features = message["features"].as_array().ok_or_else(|| {
            format!(
                "refusing to stage a patcher-less Detcore Tool image as \
                 {CANONICAL_TOOL_IMAGE}: reverie-liteinst rlib on line {} has \
                 unexpected features {}",
                index + 1,
                message["features"]
            )
        })?;
        let mut has_liteinst = false;
        for feature in features {
            match feature.as_str() {
                Some("liteinst") => has_liteinst = true,
                Some(_) => {}
                None => {
                    return Err(format!(
                        "refusing to stage a patcher-less Detcore Tool image as \
                         {CANONICAL_TOOL_IMAGE}: reverie-liteinst rlib on line {} has \
                         unexpected features {}",
                        index + 1,
                        message["features"]
                    ));
                }
            }
        }
        saw_rlib = true;
        if !has_liteinst {
            return Err(format!(
                "refusing to stage a trap-only (patcher-less) Detcore Tool image as \
                 {CANONICAL_TOOL_IMAGE}: reverie-liteinst rlib on line {} was built \
                 without the liteinst (site_patching) feature: {}",
                index + 1,
                message["features"]
            ));
        }
    }
    require(
        saw_rlib,
        "refusing to stage a patcher-less Detcore Tool image as \
         libdetcore_liteinst.so: no reverie-liteinst rlib in current Cargo output",
    )?;
    Ok("liteinst")
}

fn close<T>(result: proof::Result<T>, closing: proof::Result<()>) -> proof::Result<T> {
    match (result, closing) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(closing)) => {
            Err(format!("{error}; packaged source close failed: {closing}"))
        }
    }
}

fn run() -> proof::Result<()> {
    let mut args = std::env::args().skip(1);
    let authority = args.next().ok_or("expected --git or --source-copy")?;
    require(
        matches!(authority.as_str(), "--git" | "--source-copy"),
        "expected --git or --source-copy",
    )?;
    let target = args.next().ok_or("expected existing target directory")?;
    require(args.next().is_none(), "unexpected finalizer arguments")?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("Hermit root missing")?;
    let root = fs::canonicalize(root).map_err(|e| e.to_string())?;
    let target = fs::canonicalize(target).map_err(|e| e.to_string())?;
    require(target.is_dir(), "target directory does not exist")?;
    let resources = target.join("install_pkg/rsrcs");
    require(
        resources.is_dir(),
        "run the outer hermit-install build before finalization",
    )?;
    let installed = resources.join("libdetcore_liteinst.so");
    let receipt = resources.join("libdetcore_liteinst.package.json");
    // A failed finalization must not advertise a previous successful preload.
    for file in [&installed, &receipt] {
        match fs::remove_file(file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let serial = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let evidence = target.join(format!(
        "ci/packaged-detcore/{}-{serial}",
        std::process::id()
    ));
    fs::create_dir_all(&evidence).map_err(|e| e.to_string())?;
    let mut runner = proof::Runner::new(evidence.join("logs"), Duration::from_secs(900), 64 << 20)?;
    let manifest = root.join("Cargo.toml");
    let pin = parse_reverie_pin(
        &fs::read_to_string(root.join("detcore/Cargo.toml")).map_err(|e| e.to_string())?,
    )
    .ok_or("Detcore manifest must name one full Reverie pin")?;
    let expectation = proof::CompiledSourceExpectation {
        layout: proof::CompiledSourceLayout::HermitGit,
        image_kind: proof::FixedImageKind::Detcore,
        workspace_root: &root,
        manifest: &manifest,
        reverie_pin: &pin,
    };
    // Explicit authority; a failed Git resolution never downgrades to content.
    let sources = if authority == "--git" {
        proof::VerifiedCompiledSources::resolve(&expectation, &mut runner)?
    } else {
        proof::VerifiedCompiledSources::resolve_content(&expectation, &mut runner)?
    };
    let pending = resources.join(format!(".libdetcore_liteinst.so.{serial}"));
    let result = (|| {
        let profile_inputs = proof::FileIdentity::read(&manifest)?;
        let profile_document: toml::Value = fs::read_to_string(&manifest)
            .map_err(|e| e.to_string())?
            .parse::<toml::Value>()
            .map_err(|e| e.to_string())?;
        let profile = serde_json::to_value(
            profile_document
                .get("profile")
                .ok_or("Cargo profiles missing")?,
        )
        .map_err(|e| e.to_string())?;
        require(
            profile["liteinst-runtime"]["inherits"].as_str() == Some("validate")
                && profile["liteinst-runtime"]["lto"].as_bool() == Some(false)
                && profile["validate"]["inherits"].as_str() == Some("release")
                && profile["validate"]["debug-assertions"].as_bool() == Some(true)
                && profile["validate"]["overflow-checks"].as_bool() == Some(true),
            "ordinary runtime must use the reviewed liteinst-runtime profile",
        )?;
        let source_record = serde_json::to_value(sources.record()).map_err(|e| e.to_string())?;
        let reverie_root = source_record["reverie_root"]
            .as_str()
            .ok_or("resolved Reverie compiler root missing")?;
        let qualifier_source = proof::FileIdentity::read(
            &Path::new(reverie_root).join("reverie-liteinst-artifact/src/lib.rs"),
        )?;
        require(
            qualifier_source.sha256 == proof::qualifier_source_sha256(),
            "compiled qualifier differs from Cargo-resolved proof source",
        )?;
        let package = sources.package();
        require(
            package.name == "detcore-liteinst"
                && package.target == "detcore_liteinst"
                && package.source.is_none()
                && package.kind == ["cdylib"]
                && package.crate_types == ["cdylib"],
            "fresh metadata selected a different Detcore runtime",
        )?;
        let cargo_args = vec![
            "build".into(),
            "--locked".into(),
            "--offline".into(),
            "--manifest-path".into(),
            manifest.to_string_lossy().into_owned(),
            "-p".into(),
            "detcore-liteinst".into(),
            "--lib".into(),
            "--no-default-features".into(),
            "--profile".into(),
            "liteinst-runtime".into(),
            "--target-dir".into(),
            target.to_string_lossy().into_owned(),
            "--message-format=json-render-diagnostics".into(),
        ];
        let cargo_version = runner.text("cargo", &["--version", "--verbose"], &root)?;
        let rustc_version = runner.text("rustc", &["--version", "--verbose"], &root)?;
        let output = runner.run("cargo", &cargo_args, &root)?;
        let messages = std::str::from_utf8(&output).map_err(|e| e.to_string())?;
        let expected_profile = json!({"opt_level":"3", "debuginfo":0,
            "debug_assertions":true,"overflow_checks":true,"test":false});
        let artifact = proof::select_artifact(messages, package, &[], &expected_profile, &target)?;
        require(
            artifact.reported_path == target.join("liteinst-runtime/libdetcore_liteinst.so"),
            "Cargo reported a different ordinary runtime output",
        )?;
        // The canonical Tool image must be the site-patching (liteinst)
        // image. Refuse a trap-only build by name here, before constructor
        // qualification and before anything is published as
        // libdetcore_liteinst.so; qualification of an unpatching image
        // must never be read as site_patching evidence.
        let site_patching = site_patching_from_cargo_messages(messages)?;
        // Bind the actual current build as its qualification witness. The
        // retained command is captured immediately after the runtime build,
        // before any later retained command can replace it. This mirrors
        // Hermit 4704's stage-detcore-liteinst rebinding.
        let build_command = runner
            .commands
            .last()
            .ok_or("packaged runtime build command was not retained")?
            .clone();
        let sources =
            sources.with_current_build(&build_command, &artifact.reported_path, &target)?;
        let qualification = proof::qualify_detcore_constructor_with_sources(
            &artifact.reported_path,
            &mut runner,
            &sources,
        )?;
        qualifier_source.verify()?;
        profile_inputs.verify()?;
        qualification.artifact.verify()?;
        proof::write_new(
            &pending,
            &fs::read(&artifact.reported_path).map_err(|e| e.to_string())?,
        )?;
        fs::set_permissions(
            &pending,
            fs::metadata(&artifact.reported_path)
                .map_err(|e| e.to_string())?
                .permissions(),
        )
        .map_err(|e| e.to_string())?;
        let staged = proof::FileIdentity::read(&pending)?;
        require(
            staged.sha256 == qualification.artifact.sha256,
            "installed copy differs from qualified runtime",
        )?;
        qualification.artifact.verify()?;
        Ok(
            json!({"schema":"hermit-packaged-detcore/v1", "reverie_pin":pin,
            "site_patching":site_patching,
            "profile_name":"liteinst-runtime", "profile_inputs":profile_inputs,
            "profile_objects":profile, "cargo_argv":cargo_args,"cargo_version":cargo_version,
            "rustc_version":rustc_version,"cargo_artifact":artifact,
            "qualifier_source":qualifier_source,"compiled_qualifier_sha256":proof::qualifier_source_sha256(),
            "compiled_sources":sources.record(),"qualification":qualification}),
        )
    })();
    let result = close(result, sources.verify_unchanged(&mut runner));
    let mut record = match result {
        Ok(record) => record,
        Err(error) => {
            let _ = fs::remove_file(&pending);
            return Err(error);
        }
    };
    fs::rename(&pending, &installed).map_err(|e| e.to_string())?;
    let publication = (|| {
        let installed_identity = proof::FileIdentity::read(&installed)?;
        require(
            record["qualification"]["artifact"]["sha256"] == installed_identity.sha256,
            "installed bytes changed before receipt publication",
        )?;
        record["installed"] =
            serde_json::to_value(installed_identity).map_err(|e| e.to_string())?;
        sources.verify_unchanged(&mut runner)?;
        record["source_close_completed"] = json!(true);
        record["build_environment"] = json!(
            [
                "CARGO_BUILD_JOBS",
                "RUSTC",
                "RUSTC_WRAPPER",
                "RUSTC_WORKSPACE_WRAPPER",
                "RUSTFLAGS",
                "CARGO_ENCODED_RUSTFLAGS",
                "RUSTUP_TOOLCHAIN",
                "CARGO_HOME",
                "CC",
                "CXX",
                "AR"
            ]
            .into_iter()
            .map(|name| (name.to_owned(), std::env::var(name).ok()))
            .collect::<std::collections::BTreeMap<_, _>>()
        );
        record["commands"] = serde_json::to_value(&runner.commands).map_err(|e| e.to_string())?;
        proof::write_new(
            &receipt,
            &serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    })();
    if let Err(error) = close(publication, sources.verify_unchanged(&mut runner)) {
        let _ = fs::remove_file(&installed);
        let _ = fs::remove_file(&receipt);
        return Err(error);
    }
    println!("qualified packaged Detcore: {}", installed.display());
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("stage-detcore-liteinst: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rlib_message(features: Value) -> String {
        json!({
            "reason": "compiler-artifact",
            "target": {"name": "reverie_liteinst", "kind": ["rlib"], "crate_types": ["rlib"]},
            "features": features,
            "filenames": ["/isolated/libreverie_liteinst.rlib"]
        })
        .to_string()
    }

    #[test]
    fn canonical_tool_image_refuses_trap_only_and_patcher_less_builds_by_name() {
        // Trap-only: the rlib compiled without the liteinst feature.
        let trap = site_patching_from_cargo_messages(&rlib_message(json!([]))).unwrap_err();
        assert!(
            trap.to_string().contains("libdetcore_liteinst.so")
                && trap.to_string().contains("trap-only"),
            "{trap}"
        );
        let fixture_only =
            site_patching_from_cargo_messages(&rlib_message(json!(["allocator-fixture"])))
                .unwrap_err();
        assert!(
            fixture_only.to_string().contains("libdetcore_liteinst.so"),
            "{fixture_only}"
        );
        // Patcher-less evidence: no reverie-liteinst rlib at all, malformed
        // features, and mixed rlib messages all refuse by the same name.
        for messages in [
            String::new(),
            rlib_message(Value::Null),
            rlib_message(json!([42])),
            format!(
                "{}\n{}\n",
                rlib_message(json!(["liteinst"])),
                rlib_message(json!([]))
            ),
        ] {
            let refused = site_patching_from_cargo_messages(&messages).unwrap_err();
            assert!(
                refused.to_string().contains("libdetcore_liteinst.so"),
                "{refused}"
            );
        }
        // The patching image is admitted and labelled explicitly; extra
        // core features do not refuse (membership, not an exact array).
        assert_eq!(
            site_patching_from_cargo_messages(&rlib_message(json!(["liteinst"]))).unwrap(),
            "liteinst"
        );
        assert_eq!(
            site_patching_from_cargo_messages(&rlib_message(json!([
                "coordinator-rpc",
                "liteinst"
            ])))
            .unwrap(),
            "liteinst"
        );
    }

    #[test]
    fn source_close_preserves_both_failures_and_never_publishes_success() {
        assert_eq!(
            close::<()>(Err("build refused".into()), Ok(())),
            Err("build refused".into())
        );
        assert_eq!(close(Ok(()), Err("changed".into())), Err("changed".into()));
        assert_eq!(
            close::<()>(Err("proof refused".into()), Err("changed".into())),
            Err("proof refused; packaged source close failed: changed".into())
        );
    }
}
