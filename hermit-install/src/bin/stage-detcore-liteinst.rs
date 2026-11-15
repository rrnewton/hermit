//! Finalize the ordinary packaged Detcore preload after the outer Cargo build.
//! This executable must never be called by a Cargo build script.
//!
//! `--prebuilt <so>` stages an existing runtime file instead of the Cargo
//! output, but only after the unchanged fixed build and qualification have
//! run and the prebuilt bytes prove identical to the qualified artifact.
//! Any other bytes are refused: this receipt attests the fixed
//! `liteinst-runtime` build, and no second provenance can be recorded in it.
//!
//! `--buck` is PLUMBING ONLY (D1c-3, unqualified): it parses the explicit
//! genrule-supplied Buck inputs and carries the `builder=buck2` receipt arm
//! that `ci/verify-packaged-detcore.py` already pins, but it cannot qualify
//! anything yet. The compiled `reverie-liteinst-artifact` dependency is the
//! Hermit-pinned git revision, which has no Buck declared-content authority
//! (`resolve_buck` / `with_buck_build`); until a Reverie pin bump carries
//! that authority, `--buck` validates its arguments and then refuses by
//! name before touching the target directory. Qualifying actual Buck bytes
//! additionally waits on the D1c-2/D1c-6 machine-code recipes, which need a
//! real fbsource-built ELF. No Buck receipt has ever been emitted.
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use reverie_liteinst_artifact as proof;
use serde_json::json;

include!("../../../hermit-cli/reverie_pin.rs");

fn require(ok: bool, message: &str) -> proof::Result<()> {
    if ok { Ok(()) } else { Err(message.into()) }
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

/// One finalizer invocation: either a Cargo mode (`--git` / `--source-copy`,
/// optionally staging byte-identical `--prebuilt` output) or the `--buck`
/// mode that qualifies a supplied Buck `.so` directly. The modes never mix:
/// the byte-identity gate belongs to `--prebuilt` Cargo mode only, and Buck
/// mode never runs a Cargo build.
#[derive(Debug, PartialEq)]
enum Mode {
    Cargo {
        authority: String,
        target: String,
        prebuilt: Option<PathBuf>,
    },
    Buck(Box<BuckArgs>),
}

/// Explicit Buck-mode inputs, all supplied by the declaring genrule. Buck
/// mode derives nothing from `CARGO_MANIFEST_DIR` or any `Cargo.toml`: the
/// roots, pins, profile, target literals and declared source file lists are
/// arguments, so the fbsource import (which drops every manifest) can spell
/// them all.
#[derive(Debug, PartialEq)]
struct BuckArgs {
    target: PathBuf,
    artifact: PathBuf,
    workspace_root: PathBuf,
    reverie_root: PathBuf,
    reverie_pin: String,
    imported_sha: String,
    profile_bzl: PathBuf,
    buck_target: String,
    buck_platform: String,
    buck_constraints: Vec<String>,
    workspace_files: Vec<String>,
    reverie_files: Vec<String>,
}

/// Buck literals pinned with exact equality by the verifier's Buck arm in
/// `ci/verify-packaged-detcore.py`. The finalizer refuses any other spelling
/// rather than emitting a receipt its own reader would reject.
const BUCK_TARGET: &str = "//hermetic_infra/hermit:detcore-liteinst[cdylib]";
const BUCK_PLATFORM: &str = "ovr_config//platform/linux:x86_64-fbcode";
const BUCK_CONSTRAINTS: [&str; 2] = ["ovr_config//cpu:x86_64", "ovr_config//os:linux"];
/// The reviewed Buck `liteinst-runtime` flag set: release opt 3 with debug
/// assertions and overflow checks on, and no `-Clto` at all (matching the
/// Cargo profile's `lto = false`, which also passes no flag).
const BUCK_RUSTC_FLAGS: [&str; 8] = [
    "-Copt-level=3",
    "-Cdebuginfo=0",
    "-Cdebug-assertions=yes",
    "-Coverflow-checks=yes",
    "-Ccodegen-units=16",
    "-Cembed-bitcode=no",
    "-Cpanic=unwind",
    "-Cstrip=none",
];
/// The one refusal `--buck` can honestly make at the current Reverie pin.
const BUCK_UNAVAILABLE: &str = "Buck authority unavailable at pinned Reverie: \
    reverie-liteinst-artifact is compiled from the Hermit-pinned git revision, \
    which carries no resolve_buck/BuckCompiledSourceExpectation/with_buck_build; \
    --buck qualification requires a Reverie pin bump to a revision carrying the \
    Buck declared-content authority, plus the D1c-2/D1c-6 recipes for real Buck \
    bytes. PLUMBING ONLY: no artifact was qualified and nothing was staged";

/// The arguments after the program name: the mode, the target directory, and
/// the mode's own inputs.
fn parse_args(args: &[String]) -> proof::Result<Mode> {
    let mut args = args.iter();
    let authority = args
        .next()
        .ok_or("expected --git, --source-copy or --buck")?
        .clone();
    let target = args
        .next()
        .ok_or("expected existing target directory")?
        .clone();
    match authority.as_str() {
        "--git" | "--source-copy" => {
            let prebuilt = match args.next().map(String::as_str) {
                None => None,
                Some("--prebuilt") => Some(PathBuf::from(
                    args.next()
                        .ok_or("expected prebuilt runtime path after --prebuilt")?,
                )),
                Some(_) => return Err("unexpected finalizer arguments".into()),
            };
            require(args.next().is_none(), "unexpected finalizer arguments")?;
            Ok(Mode::Cargo {
                authority,
                target,
                prebuilt,
            })
        }
        "--buck" => parse_buck_args(PathBuf::from(target), &mut args),
        _ => Err("expected --git, --source-copy or --buck".into()),
    }
}

fn parse_buck_args(
    target: PathBuf,
    args: &mut std::slice::Iter<'_, String>,
) -> proof::Result<Mode> {
    let mut artifact = None;
    let mut workspace_root = None;
    let mut reverie_root = None;
    let mut reverie_pin = None;
    let mut imported_sha = None;
    let mut profile_bzl = None;
    let mut buck_target = None;
    let mut buck_platform = None;
    let mut buck_constraints = Vec::new();
    let mut workspace_files = Vec::new();
    let mut reverie_files = Vec::new();
    // Every singleton may be spelled once; the three list flags repeat.
    fn set_once(slot: &mut Option<String>, value: String) -> proof::Result<()> {
        require(slot.is_none(), "duplicate --buck argument")?;
        *slot = Some(value);
        Ok(())
    }
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .cloned()
                .ok_or_else(|| format!("expected value after {flag}"))
        };
        match flag.as_str() {
            "--artifact" => set_once(&mut artifact, value()?)?,
            "--workspace-root" => set_once(&mut workspace_root, value()?)?,
            "--reverie-root" => set_once(&mut reverie_root, value()?)?,
            "--reverie-pin" => set_once(&mut reverie_pin, value()?)?,
            "--imported-sha" => set_once(&mut imported_sha, value()?)?,
            "--profile-bzl" => set_once(&mut profile_bzl, value()?)?,
            "--buck-target" => set_once(&mut buck_target, value()?)?,
            "--buck-platform" => set_once(&mut buck_platform, value()?)?,
            "--buck-constraint" => buck_constraints.push(value()?),
            "--workspace-file" => workspace_files.push(value()?),
            "--reverie-file" => reverie_files.push(value()?),
            _ => return Err("unexpected --buck argument".into()),
        }
    }
    let required =
        |slot: Option<String>, flag: &str| slot.ok_or_else(|| format!("--buck requires {flag}"));
    Ok(Mode::Buck(Box::new(BuckArgs {
        target,
        artifact: PathBuf::from(required(artifact, "--artifact")?),
        workspace_root: PathBuf::from(required(workspace_root, "--workspace-root")?),
        reverie_root: PathBuf::from(required(reverie_root, "--reverie-root")?),
        reverie_pin: required(reverie_pin, "--reverie-pin")?,
        imported_sha: required(imported_sha, "--imported-sha")?,
        profile_bzl: PathBuf::from(required(profile_bzl, "--profile-bzl")?),
        buck_target: required(buck_target, "--buck-target")?,
        buck_platform: required(buck_platform, "--buck-platform")?,
        buck_constraints,
        workspace_files,
        reverie_files,
    })))
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Argument checks that need no source authority and no filesystem: the pins
/// are full lowercase-hex SHAs, the Buck literals are exactly the verifier's,
/// the profile input names the reviewed `.bzl`, and the declared file lists
/// are nonempty logical relative paths naming at least the leaf and the
/// qualifier sources the closed factory will hash.
fn validate_buck_args(args: &BuckArgs) -> proof::Result<()> {
    require(
        is_full_sha(&args.reverie_pin),
        "--buck requires a full 40-hex --reverie-pin",
    )?;
    require(
        is_full_sha(&args.imported_sha),
        "--buck requires a full 40-hex --imported-sha",
    )?;
    require(
        args.buck_target == BUCK_TARGET,
        "--buck-target is not the pinned Buck runtime target",
    )?;
    require(
        args.buck_platform == BUCK_PLATFORM,
        "--buck-platform is not the pinned Buck runtime platform",
    )?;
    require(
        args.buck_constraints == BUCK_CONSTRAINTS,
        "--buck-constraint values are not the pinned runtime constraints",
    )?;
    require(
        args.profile_bzl
            .file_name()
            .is_some_and(|name| name == "liteinst-runtime-profile.bzl"),
        "--profile-bzl is not the liteinst-runtime profile .bzl",
    )?;
    for files in [&args.workspace_files, &args.reverie_files] {
        require(!files.is_empty(), "--buck requires declared source files")?;
        for file in files {
            let path = Path::new(file);
            require(
                !file.is_empty()
                    && !path.is_absolute()
                    && path
                        .components()
                        .all(|component| matches!(component, std::path::Component::Normal(_))),
                "--buck declared files must be logical relative paths",
            )?;
        }
    }
    require(
        args.workspace_files
            .iter()
            .any(|file| file == "detcore-liteinst/src/lib.rs"),
        "--buck workspace files must declare the detcore-liteinst leaf source",
    )?;
    require(
        args.reverie_files
            .iter()
            .any(|file| file == "reverie-liteinst-artifact/src/lib.rs"),
        "--buck reverie files must declare the qualifier source",
    )?;
    Ok(())
}

fn buck_argv() -> Vec<String> {
    vec![
        "build".into(),
        BUCK_TARGET.into(),
        "--target-platforms".into(),
        BUCK_PLATFORM.into(),
    ]
}

fn buck_profile_objects() -> serde_json::Value {
    json!({"liteinst-runtime": {"rustc_flags": BUCK_RUSTC_FLAGS}})
}

/// The `buck_artifact` mirror of the Cargo receipt's artifact record: the
/// ordinary (empty) feature mode, the exact compiler profile the Buck flag
/// set produces, the synthesized leaf package, and the Buck output path the
/// qualification proof must name. There is no `fresh`: Buck has no such
/// notion to report honestly.
#[allow(dead_code)] // Wired by run_buck at the Reverie pin bump; unit-tested now.
fn buck_artifact(package: serde_json::Value, reported_path: &Path) -> serde_json::Value {
    json!({
        "features": [],
        "profile": {"opt_level": "3", "debuginfo": 0, "debug_assertions": true,
                    "overflow_checks": true, "test": false},
        "package": package,
        "reported_path": reported_path.to_string_lossy(),
    })
}

/// The `builder=buck2` receipt body, before publication adds `installed`,
/// `source_close_completed` and the executed `commands` (the qualifier's
/// readelf/objdump runs). It carries no `cargo_*` key and synthesizes no
/// Cargo value; `profile_inputs` is the profile `.bzl` identity, which is
/// also the Buck compiled-sources manifest slot, and `rustc_version` is the
/// ELF `.comment` identity read at qualification time.
#[allow(dead_code)] // Wired by run_buck at the Reverie pin bump; unit-tested now.
fn buck_receipt(
    args: &BuckArgs,
    profile_inputs: serde_json::Value,
    artifact: serde_json::Value,
    qualifier_source: serde_json::Value,
    compiled_sources: serde_json::Value,
    qualification: serde_json::Value,
    rustc_version: &str,
) -> serde_json::Value {
    json!({
        "schema": "hermit-packaged-detcore/v1",
        "builder": "buck2",
        "reverie_pin": args.reverie_pin,
        "profile_name": "liteinst-runtime",
        "profile_inputs": profile_inputs,
        "profile_objects": buck_profile_objects(),
        "buck_target": BUCK_TARGET,
        "buck_platform": BUCK_PLATFORM,
        "buck_constraints": BUCK_CONSTRAINTS,
        "buck_argv": buck_argv(),
        "buck_artifact": artifact,
        "rustc_version": rustc_version,
        "qualifier_source": qualifier_source,
        "compiled_qualifier_sha256": proof::qualifier_source_sha256(),
        "compiled_sources": compiled_sources,
        "qualification": qualification,
    })
}

/// Buck mode: qualify the supplied Buck `.so` directly against Buck-declared
/// sources — no Cargo build and no byte-identity gate. The qualification
/// sequence, once the pinned Reverie carries the Buck authority, is:
/// `VerifiedCompiledSources::resolve_buck` over the declared roots/files,
/// `with_buck_build(artifact)` for the `BuildWitness::Buck` witness,
/// `qualify_detcore_constructor_with_sources` on the supplied artifact, then
/// stage + publish [`buck_receipt`] exactly as the Cargo arm publishes its
/// receipt. At the current pin none of those entry points exist in the
/// compiled dependency, so this validates every input it honestly can and
/// then refuses by name, before any staged file is touched.
fn run_buck(args: &BuckArgs) -> proof::Result<()> {
    validate_buck_args(args)?;
    for path in [&args.target, &args.workspace_root, &args.reverie_root] {
        require(
            fs::canonicalize(path).map_err(|e| e.to_string())?.is_dir(),
            "Buck input is not a directory",
        )?;
    }
    for path in [&args.artifact, &args.profile_bzl] {
        require(
            path.is_file() && !path.is_symlink(),
            "Buck input must be a regular non-symlink file",
        )?;
    }
    Err(BUCK_UNAVAILABLE.into())
}

fn run() -> proof::Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (authority, target, prebuilt) = match parse_args(&argv)? {
        Mode::Cargo {
            authority,
            target,
            prebuilt,
        } => (authority, target, prebuilt),
        Mode::Buck(args) => return run_buck(&args),
    };
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
        let expected_profile = json!({"opt_level":"3", "debuginfo":0,
            "debug_assertions":true,"overflow_checks":true,"test":false});
        let artifact = proof::select_artifact(
            std::str::from_utf8(&output).map_err(|e| e.to_string())?,
            package,
            &[],
            &expected_profile,
            &target,
        )?;
        require(
            artifact.reported_path == target.join("liteinst-runtime/libdetcore_liteinst.so"),
            "Cargo reported a different ordinary runtime output",
        )?;
        // The ordinary staged runtime is the liteinst-off image; bind the
        // actual current build as its no-scope qualification witness. The
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
        // A prebuilt runtime is staged in place of the Cargo output only
        // when its entire digest is the qualified one. The receipt below
        // attests the fixed build that just ran, so different bytes — for
        // example another builder's profile — are refused here by name
        // rather than recorded under that build's provenance.
        let staged_from = prebuilt.as_deref().unwrap_or(&artifact.reported_path);
        let prebuilt_identity = match &prebuilt {
            Some(path) => {
                let identity = proof::FileIdentity::read(path)?;
                require(
                    identity.sha256 == qualification.artifact.sha256,
                    "prebuilt runtime differs from the qualified fixed liteinst-runtime \
                     build; only identical bytes can be staged under this receipt",
                )?;
                Some(identity)
            }
            None => None,
        };
        proof::write_new(&pending, &fs::read(staged_from).map_err(|e| e.to_string())?)?;
        fs::set_permissions(
            &pending,
            fs::metadata(staged_from)
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
        if let Some(identity) = &prebuilt_identity {
            identity.verify()?;
        }
        Ok(
            json!({"schema":"hermit-packaged-detcore/v1", "reverie_pin":pin,
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

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn arguments_accept_an_optional_prebuilt_runtime() {
        assert_eq!(
            parse_args(&argv(&["--git", "target"])).unwrap(),
            Mode::Cargo {
                authority: "--git".to_string(),
                target: "target".to_string(),
                prebuilt: None,
            }
        );
        assert_eq!(
            parse_args(&argv(&[
                "--source-copy",
                "target",
                "--prebuilt",
                "buck-out/lib.so"
            ]))
            .unwrap(),
            Mode::Cargo {
                authority: "--source-copy".to_string(),
                target: "target".to_string(),
                prebuilt: Some(PathBuf::from("buck-out/lib.so")),
            }
        );
    }

    #[test]
    fn arguments_refuse_every_other_shape() {
        for (args, message) in [
            (vec![], "expected --git, --source-copy or --buck"),
            (
                vec!["--prebuilt", "x"],
                "expected --git, --source-copy or --buck",
            ),
            (vec!["--git"], "expected existing target directory"),
            (vec!["--buck"], "expected existing target directory"),
            (
                vec!["--git", "target", "extra"],
                "unexpected finalizer arguments",
            ),
            (
                vec!["--git", "target", "--prebuilt"],
                "expected prebuilt runtime path",
            ),
            (
                vec!["--git", "target", "--prebuilt", "x", "extra"],
                "unexpected finalizer arguments",
            ),
            // The modes never mix: no --prebuilt byte-identity gate in Buck
            // mode, and no Buck flags in Cargo mode.
            (
                vec!["--buck", "target", "--prebuilt", "x"],
                "unexpected --buck argument",
            ),
            (
                vec!["--git", "target", "--artifact", "x"],
                "unexpected finalizer arguments",
            ),
        ] {
            let error = parse_args(&argv(&args)).unwrap_err().to_string();
            assert!(error.contains(message), "{args:?}: {error}");
        }
    }

    fn buck_argv_fixture() -> Vec<&'static str> {
        vec![
            "--buck",
            "target",
            "--artifact",
            "buck-out/libdetcore_liteinst.so",
            "--workspace-root",
            "hermit",
            "--reverie-root",
            "reverie",
            "--reverie-pin",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--imported-sha",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "--profile-bzl",
            "fb-only/liteinst-runtime-profile.bzl",
            "--buck-target",
            BUCK_TARGET,
            "--buck-platform",
            BUCK_PLATFORM,
            "--buck-constraint",
            BUCK_CONSTRAINTS[0],
            "--buck-constraint",
            BUCK_CONSTRAINTS[1],
            "--workspace-file",
            "detcore-liteinst/src/lib.rs",
            "--workspace-file",
            "detcore-liteinst/build.rs",
            "--reverie-file",
            "reverie-liteinst-artifact/src/lib.rs",
        ]
    }

    fn buck_args_fixture() -> BuckArgs {
        let Mode::Buck(args) = parse_args(&argv(&buck_argv_fixture())).unwrap() else {
            panic!("fixture must parse as Buck mode");
        };
        *args
    }

    #[test]
    fn buck_arguments_are_all_explicit() {
        let args = buck_args_fixture();
        assert_eq!(args.target, PathBuf::from("target"));
        assert_eq!(
            args.artifact,
            PathBuf::from("buck-out/libdetcore_liteinst.so")
        );
        assert_eq!(args.workspace_root, PathBuf::from("hermit"));
        assert_eq!(args.reverie_root, PathBuf::from("reverie"));
        assert_eq!(args.reverie_pin, "a".repeat(40));
        assert_eq!(args.imported_sha, "b".repeat(40));
        assert_eq!(args.buck_constraints, BUCK_CONSTRAINTS);
        validate_buck_args(&args).unwrap();
    }

    #[test]
    fn buck_arguments_refuse_incomplete_and_misspelled_shapes() {
        let full = buck_argv_fixture();
        for (args, message) in [
            (vec!["--buck", "target"], "--buck requires --artifact"),
            (
                vec!["--buck", "target", "--artifact"],
                "expected value after --artifact",
            ),
            (
                vec!["--buck", "target", "--unknown", "x"],
                "unexpected --buck argument",
            ),
        ] {
            let error = parse_args(&argv(&args)).unwrap_err().to_string();
            assert!(error.contains(message), "{args:?}: {error}");
        }
        // Dropping either half of a singleton pair, or repeating it, refuses.
        for cut in [2, 4, 10, 14] {
            let mut truncated = full.clone();
            truncated.drain(cut..cut + 2);
            assert!(parse_args(&argv(&truncated)).is_err(), "cut {cut}");
        }
        let mut duplicated = full.clone();
        duplicated.extend(["--artifact", "other.so"]);
        let error = parse_args(&argv(&duplicated)).unwrap_err().to_string();
        assert!(error.contains("duplicate --buck argument"), "{error}");
    }

    #[test]
    fn buck_validation_refuses_unpinned_provenance() {
        let mut args = buck_args_fixture();
        args.reverie_pin = "abc".into();
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("full 40-hex --reverie-pin")
        );
        let mut args = buck_args_fixture();
        args.imported_sha = "B".repeat(40);
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("full 40-hex --imported-sha")
        );
        let mut args = buck_args_fixture();
        args.buck_target = "//hermetic_infra/hermit:detcore-liteinst".into();
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("pinned Buck runtime target")
        );
        let mut args = buck_args_fixture();
        args.buck_platform = "ovr_config//platform/linux:aarch64-fbcode".into();
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("pinned Buck runtime platform")
        );
        let mut args = buck_args_fixture();
        args.buck_constraints.reverse();
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("pinned runtime constraints")
        );
        let mut args = buck_args_fixture();
        args.profile_bzl = PathBuf::from("fb-only/other.bzl");
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("liteinst-runtime profile .bzl")
        );
        let mut args = buck_args_fixture();
        args.workspace_files = vec!["/absolute/lib.rs".into()];
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("logical relative paths")
        );
        let mut args = buck_args_fixture();
        args.workspace_files = vec!["detcore-liteinst/build.rs".into()];
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("leaf source")
        );
        let mut args = buck_args_fixture();
        args.reverie_files = vec!["reverie-liteinst/src/patch_alloc.rs".into()];
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("qualifier source")
        );
        let mut args = buck_args_fixture();
        args.reverie_files.clear();
        assert!(
            validate_buck_args(&args)
                .unwrap_err()
                .contains("declared source files")
        );
    }

    #[test]
    fn buck_mode_refuses_by_name_at_the_pinned_reverie() {
        // Valid arguments whose paths do not exist stop at path checks; the
        // authority refusal itself is exercised end to end by the dress
        // rehearsal against real roots. Here the named refusal text is the
        // contract: it must keep identifying the missing authority.
        assert!(BUCK_UNAVAILABLE.starts_with("Buck authority unavailable at pinned Reverie"));
        assert!(BUCK_UNAVAILABLE.contains("resolve_buck"));
        assert!(BUCK_UNAVAILABLE.contains("PLUMBING ONLY"));
        let error = run_buck(&buck_args_fixture()).unwrap_err();
        assert!(!error.is_empty());
    }

    /// The receipt body must match the verifier's Buck arm exactly: same
    /// schema, `builder=buck2`, the flag-dict profile, the Buck literals and
    /// argv, the artifact mirror, and no `cargo_*` key anywhere.
    #[test]
    fn buck_receipt_matches_the_verifier_buck_arm() {
        let args = buck_args_fixture();
        let package = json!({"name": "detcore-liteinst", "target": "detcore_liteinst",
            "source": null, "kind": ["cdylib"], "crate_types": ["cdylib"]});
        let profile_inputs = json!({"path": "/fixture/fb-only/liteinst-runtime-profile.bzl", "sha256": "1".repeat(64)});
        let record = buck_receipt(
            &args,
            profile_inputs.clone(),
            buck_artifact(
                package.clone(),
                Path::new("/fixture/buck-out/libdetcore_liteinst.so"),
            ),
            json!({"path": "/fixture/reverie/reverie-liteinst-artifact/src/lib.rs",
                   "sha256": proof::qualifier_source_sha256()}),
            json!({"image_kind": "Detcore", "reverie_pin": args.reverie_pin,
                   "manifest": profile_inputs, "leaf": package,
                   "reverie_root": "/fixture/reverie"}),
            json!({"artifact": {"path": "/fixture/buck-out/libdetcore_liteinst.so"}}),
            "rustc fixture",
        );
        let object = record.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "buck_argv",
                "buck_artifact",
                "buck_constraints",
                "buck_platform",
                "buck_target",
                "builder",
                "compiled_qualifier_sha256",
                "compiled_sources",
                "profile_inputs",
                "profile_name",
                "profile_objects",
                "qualification",
                "qualifier_source",
                "reverie_pin",
                "rustc_version",
                "schema",
            ]
        );
        assert!(keys.iter().all(|key| !key.starts_with("cargo_")));
        assert_eq!(record["schema"], "hermit-packaged-detcore/v1");
        assert_eq!(record["builder"], "buck2");
        assert_eq!(record["profile_name"], "liteinst-runtime");
        assert_eq!(
            record["profile_objects"],
            json!({"liteinst-runtime": {"rustc_flags": [
                "-Copt-level=3", "-Cdebuginfo=0", "-Cdebug-assertions=yes",
                "-Coverflow-checks=yes", "-Ccodegen-units=16",
                "-Cembed-bitcode=no", "-Cpanic=unwind", "-Cstrip=none"]}})
        );
        assert_eq!(
            record["buck_argv"],
            json!(["build", BUCK_TARGET, "--target-platforms", BUCK_PLATFORM])
        );
        assert_eq!(record["buck_target"], BUCK_TARGET);
        assert_eq!(record["buck_platform"], BUCK_PLATFORM);
        assert_eq!(record["buck_constraints"], json!(BUCK_CONSTRAINTS));
        assert_eq!(record["buck_artifact"]["features"], json!([]));
        assert_eq!(
            record["buck_artifact"]["profile"],
            json!({"opt_level": "3", "debuginfo": 0, "debug_assertions": true,
                   "overflow_checks": true, "test": false})
        );
        assert_eq!(
            record["buck_artifact"]["reported_path"],
            "/fixture/buck-out/libdetcore_liteinst.so"
        );
        // The verifier joins profile_inputs to the compiled-sources manifest
        // slot; in the Buck arm both are the profile .bzl identity.
        assert_eq!(
            record["profile_inputs"],
            record["compiled_sources"]["manifest"]
        );
        assert_eq!(
            record["compiled_qualifier_sha256"],
            proof::qualifier_source_sha256()
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
