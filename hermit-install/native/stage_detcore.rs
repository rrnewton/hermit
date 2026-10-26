//! Native Buck host entry; deliberately outside Cargo's auto-discovered bins.
//! Fresh qualification admits the source/action. The receipt only records it.
use std::fs;
use std::path::Path;
use std::time::Duration;

use reverie_liteinst_artifact as proof;
use serde_json::json;

mod inputs;
#[path = "../../liteinst-runtime-build/native_bundle.rs"]
mod native_bundle;
use inputs::Inputs;
use inputs::require;
use native_bundle::NativeBundlePublication;
use native_bundle::copy_into_protected_stage;

fn install(
    inputs: &Inputs,
    resources: &Path,
    stage: &Path,
    runner: &mut proof::Runner,
) -> proof::Result<()> {
    require(
        resources.is_dir(),
        "native install resources directory must exist",
    )?;
    require(
        stage.parent() == Some(resources),
        "native install stage must be an immediate resources child",
    )?;
    let installed = resources.join("libdetcore_liteinst.so");
    let receipt = resources.join("libdetcore_liteinst.native-package.json");
    // A graph action owns fresh outputs. Never remove a previous producer's files.
    require(
        fs::symlink_metadata(&installed).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            && fs::symlink_metadata(&receipt)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "native install outputs already exist or cannot be inspected",
    )?;
    let mut publication = NativeBundlePublication::create(stage, true)?;
    inputs.verify()?;
    let qualified = proof::native_action::qualify_declared_constructor(
        &inputs.expected,
        &inputs.declared,
        runner,
    )?;
    let artifact = qualified.qualification().artifact();
    require(
        artifact == qualified.compilation().product(),
        "native proof and selected compiler output differ",
    )?;
    artifact.verify()?;
    let pending = publication.directory().join("libdetcore_liteinst.so");
    copy_into_protected_stage(&artifact.path, &pending).map_err(|e| e.to_string())?;
    require(
        proof::FileIdentity::read(&pending)?.sha256 == artifact.sha256,
        "native installed copy differs from qualified runtime",
    )?;
    artifact.verify()?;
    publication.rename("libdetcore_liteinst.so")?;
    let installed_identity = proof::FileIdentity::read(&installed)?;
    require(
        installed_identity.sha256 == artifact.sha256,
        "published native runtime differs",
    )?;
    // Rebind again after publication: a completed record/digest cannot keep
    // mutable source/producer authority alive during the copy transaction.
    let closed = proof::native_action::qualify_declared_constructor(
        &inputs.expected,
        &inputs.declared,
        runner,
    )?;
    require(
        closed.qualification().artifact() == artifact
            && closed.qualification().source_sha256() == qualified.qualification().source_sha256()
            && closed.compilation().evidence() == qualified.compilation().evidence(),
        "native source or producer changed during installation",
    )?;
    inputs.verify()?;
    installed_identity.verify()?;
    let record = json!({
        "schema": "hermit-native-packaged-detcore/v1",
        "native_expected": inputs.expected_file,
        "native_declared": inputs.declared_file,
        "native_compilation": {"product": closed.compilation().product(),
            "evidence": closed.compilation().evidence(), "reused": closed.compilation().reused()},
        "qualification": closed.qualification(),
        "installed": installed_identity,
        "source_close_completed": true,
        "commands": runner.commands,
    });
    // Receipt is the last output. It is observation data, never rebind authority.
    let receipt_bytes = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
    proof::write_new(
        &stage.join("libdetcore_liteinst.native-package.json"),
        &receipt_bytes,
    )?;
    publication.rename("libdetcore_liteinst.native-package.json")?;
    require(
        fs::read(&receipt).map_err(|e| e.to_string())? == receipt_bytes,
        "published native receipt differs",
    )?;
    installed_identity.verify()?;
    publication.complete(true)?;
    Ok(())
}

fn run() -> proof::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!(
            "Usage: stage-native-detcore --expected JSON --declared JSON --resources RESOURCES --stage NEW_STAGE --logs NEW_LOGS\nFreshly qualify and install ordinary native Detcore; publish a native receipt last.\nAll outputs must be fresh. This entry is built by the native Buck graph, not Cargo."
        );
        return Ok(());
    }
    let options = inputs::options(
        args,
        &[
            "--expected",
            "--declared",
            "--resources",
            "--stage",
            "--logs",
        ],
    )?;
    let inputs = Inputs::read(
        Path::new(&options["--expected"]),
        Path::new(&options["--declared"]),
    )?;
    let mut runner = proof::Runner::new(
        Path::new(&options["--logs"]).to_owned(),
        Duration::from_secs(900),
        64 << 20,
    )?;
    install(
        &inputs,
        Path::new(&options["--resources"]),
        Path::new(&options["--stage"]),
        &mut runner,
    )
}
fn main() {
    if let Err(error) = run() {
        eprintln!("stage-native-detcore: {error}");
        std::process::exit(2);
    }
}
