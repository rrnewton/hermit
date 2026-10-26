//! Native Buck entry for the existing eight-file Hermit native-exec bundle.
//! Compile this source against the reviewed native R API in the native graph.
//! It is intentionally absent from Cargo bins while Cargo remains pinned to R33.
//! The native graph supplies --cfg feature="native-exec" and the direct
//! artifact, inguest, detcore_model, serde, serde_json, libc, goblin and sha2 crates.
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use reverie_liteinst_artifact as proof;
use serde_json::json;
mod inputs;
#[path = "../../liteinst-runtime-build/native_bundle.rs"]
mod native_bundle;
use inputs::BundleInputs;
use inputs::Inputs;
use inputs::require;
use native_bundle::NativeBundlePublication;
use native_bundle::copy_into_protected_stage;
use native_bundle::physical::validate_bundle;

const SOURCES: [&str; 8] = [
    "private-bootstrap.c",
    "private-control.h",
    "private-entry.S",
    "private-fs.S",
    "private-kernel-state.c",
    "private-maps.c",
    "private-bootstrap.ld",
    "private-runner.c",
];
// Compare against the exact table consumed by R's compiled source digest.
// Expected graph hashes alone cannot join C inputs to that compiled digest.
fn source_files(
    files: &BTreeMap<String, proof::FileIdentity>,
) -> proof::Result<BTreeMap<String, proof::FileIdentity>> {
    SOURCES
        .iter()
        .map(|name| {
            let key = format!("rv/bootstrap/{name}");
            let file = files
                .get(&key)
                .ok_or_else(|| format!("missing declared native source {key}"))?;
            Ok((key, file.clone()))
        })
        .collect()
}
fn verify_source_bytes(
    files: &BTreeMap<String, proof::FileIdentity>,
    embedded: &[(&str, &[u8])],
) -> proof::Result<()> {
    require(
        files.len() == SOURCES.len() && embedded.len() == SOURCES.len(),
        "native bootstrap source membership differs from compiled digest inputs",
    )?;
    for name in SOURCES {
        let logical = format!("rv/bootstrap/{name}");
        let matches: Vec<_> = embedded.iter().filter(|(key, _)| *key == logical).collect();
        require(
            matches.len() == 1,
            "native bootstrap source names differ from compiled digest inputs",
        )?;
        let file = files
            .get(&logical)
            .ok_or("native bootstrap source key differs from compiled digest inputs")?;
        inputs::verify_source_file_bytes(file, matches[0].1)?;
    }
    Ok(())
}

fn verify_bundle_inputs(
    inputs: &BundleInputs,
    files: &BTreeMap<String, proof::FileIdentity>,
) -> proof::Result<()> {
    // Resolve the original declared paths again, including their no-follow
    // checks. Rehashing only cached canonical paths can miss a changed binding.
    let current = inputs.files()?;
    require(
        current == *files,
        "declared native bundle input identities changed",
    )?;
    verify_source_bytes(
        &source_files(&current)?,
        reverie_inguest::native_exec_sources::bootstrap_sources(),
    )
}

fn verify_runner_runtime(
    files: &BTreeMap<String, proof::FileIdentity>,
    artifact: &proof::FileIdentity,
) -> proof::Result<()> {
    require(
        files.get("runner_runtime") == Some(artifact),
        "runner link input differs from the qualified native runtime",
    )
}

fn stage_product(
    inputs: &BundleInputs,
    key: &str,
    original: &proof::FileIdentity,
    destination: &Path,
) -> proof::Result<proof::FileIdentity> {
    copy_into_protected_stage(&original.path, destination).map_err(|e| e.to_string())?;
    // The copy has its own path. Bind its bytes and size to the selected
    // product snapshot, not its pathname to the original Buck output path.
    let copied = inputs.verify_copy(key, destination)?;
    require(
        copied.sha256 == original.sha256,
        "staged native product differs from the selected Buck Artifact",
    )?;
    Ok(copied)
}

fn stage(
    inputs: &Inputs,
    bundle_inputs: &BundleInputs,
    directory: &Path,
    logs: &Path,
) -> proof::Result<()> {
    // Bind publication to the caller's real output parent. Bootstrap/runner
    // compilation belongs to their normal declared Buck producers.
    let parent = directory
        .parent()
        .ok_or("native bundle stage has no output parent")?;
    let name = directory
        .file_name()
        .ok_or("native bundle stage has no filename")?;
    let directory = fs::canonicalize(if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    })
    .map_err(|e| format!("native output parent must exist: {e}"))?
    .join(name);
    let directory = directory.as_path();
    let mut authority_runner =
        proof::Runner::new(logs.join("authority"), Duration::from_secs(1200), 16 << 20)?;
    // This guard outlives the callback and stays armed through R's final close.
    let mut publication = NativeBundlePublication::create(directory, true)?;
    inputs.verify()?;
    let files = bundle_inputs.files()?;
    let sources = source_files(&files)?;
    verify_source_bytes(
        &sources,
        reverie_inguest::native_exec_sources::bootstrap_sources(),
    )?;
    let record = proof::native_action::with_declared_native_entry(
        &inputs.expected,
        &inputs.declared,
        &mut authority_runner,
        |scope| {
            let qualification = scope.qualification();
            require(
                qualification.artifact() == scope.compilation().product(),
                "native entry proof differs from selected current compiler output",
            )?;
            verify_bundle_inputs(bundle_inputs, &files)?;
            verify_runner_runtime(&files, qualification.artifact())?;
            let bootstrap = files
                .get("bootstrap")
                .ok_or("missing declared bootstrap Artifact")?;
            let runner_file = files
                .get("runner")
                .ok_or("missing declared runner Artifact")?;
            let bootstrap_copy = stage_product(
                bundle_inputs,
                "bootstrap",
                bootstrap,
                &directory.join("private-bootstrap"),
            )?;
            let runner_copy = stage_product(
                bundle_inputs,
                "runner",
                runner_file,
                &directory.join("private-runner"),
            )?;
            let runtime = directory.join("libdetcore_liteinst.so");
            copy_into_protected_stage(&qualification.artifact().path, &runtime)
                .map_err(|e| e.to_string())?;
            let source_digest = detcore_model::liteinst_native_exec_sources::digest(
                reverie_inguest::native_exec_sources::digest(),
            );
            let bundle = validate_bundle(
                &bootstrap_copy.path,
                &runner_copy.path,
                &runtime,
                qualification.artifact(),
                qualification.generic_bootstrap(),
                source_digest,
                || {
                    verify_bundle_inputs(bundle_inputs, &files)?;
                    verify_runner_runtime(&files, qualification.artifact())?;
                    inputs.verify()?;
                    scope.verify_unchanged()
                },
            )?;
            require(
                bundle.bootstrap() == &bootstrap_copy && bundle.runner() == &runner_copy,
                "validated native products differ from the checked owned copies",
            )?;
            bundle.publish(&mut publication)?;
            // Serializable observation only; it cannot reconstruct the scope.
            Ok(json!({"schema": "hermit-native-exec-bundle/v1",
                "native_expected": inputs.expected_file, "native_declared": inputs.declared_file,
                "native_compilation": {"product": scope.compilation().product(),
                    "evidence": scope.compilation().evidence(), "reused": scope.compilation().reused()},
                "qualification": qualification, "native_sources": sources,
                "runner_runtime": files["runner_runtime"],
                "bundle_expected": bundle_inputs.expected_file, "bundle_declared": bundle_inputs.declared_file,
                "source_digest": bundle.source_digest(), "gate_digest": bundle.gate_digest(),
                "bootstrap": bundle.bootstrap(), "runner": bundle.runner(), "runtime": bundle.runtime()}))
        },
    )?;
    inputs.verify()?;
    verify_bundle_inputs(bundle_inputs, &files)?;
    let mut record = record;
    record["source_close_completed"] = json!(true);
    record["authority_commands"] = json!(authority_runner.commands);
    let bytes = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
    proof::write_new(&directory.join("native-qualification.json"), &bytes)?;
    // The final receipt is retained in the owned stage, outside the eight OUT_DIR outputs.
    require(
        fs::read(directory.join("native-qualification.json")).map_err(|e| e.to_string())? == bytes,
        "native qualification receipt readback differs",
    )?;
    publication.complete(false)?;
    Ok(())
}
fn run() -> proof::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!(
            "Usage: stage-native-exec --expected JSON --declared JSON --bundle-expected JSON --bundle-declared JSON --stage NEW_STAGE --logs NEW_LOGS\nQualify explicit native Detcore entry, verify declared Buck bootstrap/runner Artifacts, and publish the existing eight-file OUT_DIR bundle. Requires the native Buck graph and reviewed R native-entry API."
        );
        return Ok(());
    }
    let options = inputs::options(
        args,
        &[
            "--expected",
            "--declared",
            "--bundle-expected",
            "--bundle-declared",
            "--stage",
            "--logs",
        ],
    )?;
    let inputs = Inputs::read(
        Path::new(&options["--expected"]),
        Path::new(&options["--declared"]),
    )?;
    let mut keys = vec![
        "bootstrap".to_owned(),
        "runner".to_owned(),
        "runner_runtime".to_owned(),
    ];
    keys.extend(SOURCES.iter().map(|name| format!("rv/bootstrap/{name}")));
    let bundle_inputs = BundleInputs::read(
        Path::new(&options["--bundle-expected"]),
        Path::new(&options["--bundle-declared"]),
        &keys,
    )?;
    stage(
        &inputs,
        &bundle_inputs,
        Path::new(&options["--stage"]),
        Path::new(&options["--logs"]),
    )
}
fn main() {
    if let Err(error) = run() {
        eprintln!("stage-native-exec: {error}");
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "h-native-source-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn staged_product_preserves_selected_bytes_and_refuses_changed_copy_or_replacement() {
        // This tests copying data under the native publication guard. It
        // cannot produce a runtime qualification or validate an ELF bundle.
        let fixture = Fixture::new();
        let source = fixture.0.join("selected-bootstrap");
        let bytes = b"selected producer output";
        fs::write(&source, bytes).unwrap();
        let expected = fixture.0.join("expected.json");
        let declared = fixture.0.join("declared.json");
        fs::write(
            &expected,
            serde_json::to_vec(&json!({"bootstrap": {
                "sha256": proof::sha256(bytes), "size": bytes.len()
            }}))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            &declared,
            serde_json::to_vec(&json!({"bootstrap": source})).unwrap(),
        )
        .unwrap();
        // BundleInputs also serves this narrow copy test; run() independently
        // requires all eleven actual graph keys.
        let inputs = BundleInputs::read(&expected, &declared, &["bootstrap".into()]).unwrap();
        let selected = inputs.files().unwrap().remove("bootstrap").unwrap();
        let directory = fixture.0.join("owned-stage");
        let publication = NativeBundlePublication::create(&directory, true).unwrap();
        let copied_path = directory.join("private-bootstrap");
        let copied = stage_product(&inputs, "bootstrap", &selected, &copied_path).unwrap();
        assert_ne!(copied.path, selected.path);
        assert_eq!(copied.sha256, selected.sha256);
        assert_eq!(fs::read(&copied_path).unwrap(), bytes);
        // Source replacement cannot change the owned copy; a second copy is
        // refused both by its content pin and by create-new on an existing name.
        let changed = vec![b'X'; bytes.len()];
        fs::write(&source, &changed).unwrap();
        assert_eq!(fs::read(&copied_path).unwrap(), bytes);
        assert!(stage_product(&inputs, "bootstrap", &selected, &copied_path).is_err());
        assert_eq!(fs::read(&copied_path).unwrap(), bytes);
        assert!(
            stage_product(&inputs, "bootstrap", &selected, &directory.join("changed"))
                .unwrap_err()
                .contains("bytes differ")
        );
        // Restoration does not authorize a corrupted owned copy.
        fs::write(&source, bytes).unwrap();
        fs::write(&copied_path, &changed).unwrap();
        assert!(inputs.verify_copy("bootstrap", &copied_path).is_err());
        drop(publication);
        assert!(!directory.exists());
        assert_eq!(fs::read(&source).unwrap(), bytes);
    }
    #[test]
    fn runner_link_input_must_be_the_qualified_runtime_identity() {
        // File identities here exercise only the bundle join; they cannot
        // construct the opaque R native-entry qualification used by stage().
        let runtime = proof::FileIdentity {
            path: PathBuf::from("/declared/native-runtime.so"),
            sha256: proof::sha256(b"runtime"),
        };
        let files = BTreeMap::from([("runner_runtime".into(), runtime.clone())]);
        verify_runner_runtime(&files, &runtime).unwrap();
        let mut other = runtime.clone();
        other.path = PathBuf::from("/other/same-bytes.so");
        assert!(verify_runner_runtime(&files, &other).is_err());
        other = runtime.clone();
        other.sha256 = proof::sha256(b"other runtime");
        assert!(verify_runner_runtime(&files, &other).is_err());
        assert!(verify_runner_runtime(&BTreeMap::new(), &runtime).is_err());
    }
    #[test]
    fn c_source_join_accepts_exact_bytes_and_refuses_changed_or_swapped_sources() {
        // These are content-join fixtures, never constructor/native authority.
        let fixture = Fixture::new();
        let names: Vec<_> = SOURCES
            .iter()
            .map(|name| format!("rv/bootstrap/{name}"))
            .collect();
        let bytes: Vec<_> = names
            .iter()
            .map(|name| format!("compiled {name}").into_bytes())
            .collect();
        let embedded: Vec<_> = names
            .iter()
            .zip(&bytes)
            .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
            .collect();
        let files: BTreeMap<_, _> = SOURCES
            .iter()
            .zip(&embedded)
            .map(|(filename, (logical, bytes))| {
                let path = fixture.0.join(filename);
                fs::write(&path, bytes).unwrap();
                (
                    (*logical).to_owned(),
                    proof::FileIdentity::read(&path).unwrap(),
                )
            })
            .collect();
        verify_source_bytes(&files, &embedded).unwrap();
        for (logical, original) in &embedded {
            let file = &files[*logical];
            fs::write(&file.path, b"different source").unwrap();
            // Even an independently pinned changed file must match the
            // corresponding bytes consumed by the compiled source digest.
            let changed: BTreeMap<_, _> = files
                .iter()
                .map(|(key, f)| (key.clone(), proof::FileIdentity::read(&f.path).unwrap()))
                .collect();
            assert!(
                verify_source_bytes(&changed, &embedded)
                    .unwrap_err()
                    .contains("compiled digest inputs")
            );
            fs::write(&file.path, original).unwrap();
        }
        let mut missing = files.clone();
        missing.remove(&names[0]);
        assert!(verify_source_bytes(&missing, &embedded).is_err());
        assert!(verify_source_bytes(&files, &embedded[..7]).is_err());
        let mut duplicates = embedded.clone();
        duplicates[7] = embedded[0];
        assert!(verify_source_bytes(&files, &duplicates).is_err());
        let mut swapped = files.clone();
        swapped.insert(names[0].clone(), files[&names[1]].clone());
        swapped.insert(names[1].clone(), files[&names[0]].clone());
        assert!(
            verify_source_bytes(&swapped, &embedded)
                .unwrap_err()
                .contains("source bytes differ")
        );
        verify_source_bytes(&files, &embedded).unwrap();
    }
}
