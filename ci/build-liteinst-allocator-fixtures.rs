#!/usr/bin/env -S rust-script --force
//! Build both genuine M1 diagnostic leaves and the unchanged C guest.
//! The official DAG owns CPU/memory/cgroup limits. This producer owns a shared
//! 900-second child deadline, process groups and 64-MiB command stream caps.
//! It does not run guests or replace the normal prepared Hermit/resource tree.
//! ```cargo
//! [dependencies]
//! goblin = "=0.10.7"
//! sha2 = "=0.10.9"
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! libc = "0.2"
//! detcore-model = { path = "../detcore-model" }
//! ```

#[path = "../reverie/scripts/m1_artifact.rs"]
pub mod m1_artifact;
#[path = "../hermit-cli/tests/common/m1_bundle.rs"]
mod m1_bundle;
#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use m1_artifact::ConsumerExpectation;
use m1_artifact::FileIdentity;
use m1_artifact::Result;
use m1_artifact::Runner;
use m1_artifact::RuntimeReceipt;
use m1_artifact::SourceIdentity;
use m1_bundle::Bundle;
use m1_bundle::Leaf;
use serde_json::Value;
use serde_json::json;

const POINTER: &str = "target/ci/m1-allocator-fixtures.path";
const FEATURES: [&str; 1] = ["allocator-fixture"];

fn require(ok: bool, message: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(message.to_owned()) }
}

fn cargo_profile(validate: bool) -> Value {
    json!({"opt_level":"3", "debuginfo":0, "debug_assertions":validate,
        "overflow_checks":validate, "test":false})
}

fn source(
    root: &Path,
    lock: &Path,
    oracles: &BTreeMap<String, PathBuf>,
    runner: &mut Runner,
) -> Result<SourceIdentity> {
    let identity = m1_artifact::source_identity(root, lock, oracles, runner)?;
    require(
        identity.oracles["rust_exports"].sha256 == m1_artifact::FROZEN_RUST_ORACLE
            && identity.oracles["c_workload"].sha256 == m1_artifact::FROZEN_C_ORACLE,
        "frozen Rust/C oracle changed",
    )?;
    Ok(identity)
}

fn build_leaf(
    root: &Path,
    bundle: &Path,
    oracles: &BTreeMap<String, PathBuf>,
    standalone: bool,
    deadline: Instant,
) -> Result<Leaf> {
    let name = if standalone { "standalone" } else { "detcore" };
    let own = bundle.join(name);
    fs::create_dir(&own).map_err(|e| e.to_string())?;
    let mut runner = Runner::new(
        own.join("logs"),
        deadline.saturating_duration_since(Instant::now()),
        64 * 1024 * 1024,
    )?;
    let manifest = if standalone {
        root.join("liteinst-runtime-build/runtime/Cargo.toml")
    } else {
        root.join("Cargo.toml")
    };
    let lock = if standalone {
        root.join("liteinst-runtime-build/Cargo.lock")
    } else {
        root.join("Cargo.lock")
    };
    let before = source(root, &lock, oracles, &mut runner)?;
    let metadata: Value = serde_json::from_slice(&runner.run(
        "cargo",
        &[
            "metadata".into(),
            "--locked".into(),
            "--offline".into(),
            "--format-version=1".into(),
            "--manifest-path".into(),
            manifest.to_string_lossy().into_owned(),
        ],
        root,
    )?)
    .map_err(|e| e.to_string())?;
    let (package_name, target_name, initializer, filename) = if standalone {
        (
            "reverie-liteinst-preload",
            "reverie_liteinst_preload",
            "reverie_liteinst_initialize",
            "libreverie_liteinst.so",
        )
    } else {
        (
            "detcore-liteinst",
            "detcore_liteinst",
            "detcore_liteinst_initialize",
            "libdetcore_liteinst.so",
        )
    };
    let pin = runner
        .text("git", &["rev-parse", "HEAD"], &root.join("reverie"))?
        .trim()
        .to_owned();
    let expected_source = format!("git+https://github.com/rrnewton/reverie.git?rev={pin}#{pin}");
    let package_manifest = if standalone {
        let packages = metadata["packages"]
            .as_array()
            .ok_or("Cargo metadata packages missing")?;
        let candidates: Vec<_> = packages
            .iter()
            .filter(|p| p["name"] == package_name)
            .collect();
        require(
            candidates.len() == 1,
            "one exact nested preload package required",
        )?;
        PathBuf::from(
            candidates[0]["manifest_path"]
                .as_str()
                .ok_or("nested manifest missing")?,
        )
    } else {
        root.join("detcore-liteinst/Cargo.toml")
    };
    let package = m1_artifact::select_package(
        &metadata,
        package_name,
        target_name,
        &package_manifest,
        standalone.then_some(expected_source.as_str()),
    )?;
    let target = own.join("target");
    fs::create_dir(&target).map_err(|e| format!("new dedicated target: {e}"))?;
    let profile_name = if standalone { "release" } else { "validate" };
    let profile = cargo_profile(!standalone);
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let rustc_verbose = runner.text(&rustc, &["--version", "--verbose"], root)?;
    let cargo_version = runner.text("cargo", &["--version", "--verbose"], root)?;
    let readelf_version = runner.text("readelf", &["--version"], root)?;
    let objdump_version = runner.text("objdump", &["--version"], root)?;
    let mut build_arguments = vec![
        "build".into(),
        "--locked".into(),
        "--offline".into(),
        "--manifest-path".into(),
        manifest.to_string_lossy().into_owned(),
        "-p".into(),
        package_name.into(),
        "--lib".into(),
        "--profile".into(),
        profile_name.into(),
        "--target-dir".into(),
        target.to_string_lossy().into_owned(),
        "--message-format=json-render-diagnostics".into(),
    ];
    if standalone {
        // Cargo can enable features only through a selected workspace member.
        // This existing inert member forwards the diagnostic ABI to the real
        // leaf; current-artifact selection below still requires only that leaf.
        build_arguments.extend([
            "-p".into(),
            "hermit-liteinst-runtime-artifact".into(),
            "--features".into(),
            "allocator-fixture".into(),
        ]);
    } else {
        build_arguments.extend(["--features".into(), "allocator-fixture".into()]);
    }
    let output = runner.run("cargo", &build_arguments, root)?;
    let features: Vec<String> = FEATURES.into_iter().map(str::to_owned).collect();
    let cargo_artifact = m1_artifact::select_artifact(
        std::str::from_utf8(&output).map_err(|e| e.to_string())?,
        &package,
        &features,
        &profile,
        &target,
    )?;
    let current = FileIdentity::read(&cargo_artifact.reported_path)?;
    let stage = own.join(filename);
    m1_artifact::write_new(&stage, &fs::read(&current.path).map_err(|e| e.to_string())?)?;
    current.verify()?;
    let artifact = FileIdentity::read(&stage)?;
    require(
        artifact.sha256 == current.sha256,
        "staged/current runtime bytes differ",
    )?;
    let qualification = m1_artifact::qualify_elf(&stage, initializer, &mut runner)?;
    // Preserve canonical name and the standalone loader's sidecar contract:
    // immutable unique DSO is present and qualified before the marker appears.
    if standalone {
        m1_artifact::write_new(
            &own.join(format!("{filename}.revision")),
            format!("{pin}\n").as_bytes(),
        )?;
    }
    let after = source(root, &lock, oracles, &mut runner)?;
    require(before == after, "source changed during leaf production")?;
    current.verify()?;
    artifact.verify()?;
    let receipt = RuntimeReceipt {
        schema_version: m1_artifact::SCHEMA,
        source_before: before.clone(),
        source_after: after,
        cargo_artifact,
        profile_name: profile_name.into(),
        artifact,
        qualification,
        rustc_verbose,
        cargo_version,
        readelf_version,
        objdump_version,
        build_environment: [
            "RUSTUP_TOOLCHAIN",
            "RUSTC",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_BUILD_TARGET",
            "CARGO_BUILD_JOBS",
            "CC",
            "CXX",
            "AR",
            "NIX_DONT_SET_RPATH_x86_64_unknown_linux_gnu",
            "PATH",
        ]
        .into_iter()
        .map(|name| (name.to_owned(), std::env::var(name).ok()))
        .collect(),
        target_directory: target,
        commands: runner.commands,
        executed_tests: 0,
        full_m1_pass_claimed: false,
    };
    let receipt_path = own.join("runtime.json");
    m1_artifact::write_new(
        &receipt_path,
        &serde_json::to_vec_pretty(&receipt).map_err(|e| e.to_string())?,
    )?;
    // Refuse an unpublished receipt that the unchanged consumer would reject.
    // Expectations come from the independently observed build inputs above.
    let oracle_sha256 = before
        .oracles
        .iter()
        .map(|(name, file)| (name.clone(), file.sha256.clone()))
        .collect();
    m1_artifact::verify_runtime_receipt(
        &receipt_path,
        &stage,
        &ConsumerExpectation {
            package: &package,
            features: &features,
            profile_name,
            profile: &profile,
            initializer,
            source_head: &before.head,
            source_tree: &before.tree,
            source_lock_sha256: &before.lock.sha256,
            oracle_sha256: &oracle_sha256,
        },
    )?;
    Ok(Leaf {
        runtime: stage,
        receipt: receipt_path,
        profile_name: profile_name.into(),
        cargo_profile: profile,
    })
}

fn produce(root: &Path) -> Result<()> {
    require(
        std::env::var("CARGO_NET_OFFLINE").as_deref() == Ok("true"),
        "producer requires CARGO_NET_OFFLINE=true",
    )?;
    let deadline = Instant::now() + Duration::from_secs(900);
    let parent = root.join("target/ci/m1-allocator-fixtures");
    fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
    let generation = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos()
    );
    let bundle = parent.join(generation);
    fs::create_dir(&bundle).map_err(|e| e.to_string())?;
    let bundle = fs::canonicalize(bundle).map_err(|e| e.to_string())?;
    let mut runner = Runner::new(
        bundle.join("logs"),
        Duration::from_secs(900),
        64 * 1024 * 1024,
    )?;
    let oracles = BTreeMap::from([
        (
            "rust_exports".into(),
            root.join("reverie/reverie-liteinst/src/allocator_fixture.rs"),
        ),
        (
            "c_workload".into(),
            root.join("reverie/reverie-liteinst/tests/fixtures/m1_allocator_guest.c"),
        ),
    ]);
    let before = source(root, &root.join("Cargo.lock"), &oracles, &mut runner)?;
    // H commands resolve through H's root/nested locks. RV's ignored workspace
    // lock is not an input to either H build.
    let rv_head = runner
        .text("git", &["rev-parse", "HEAD"], &root.join("reverie"))?
        .trim()
        .to_owned();
    let rv_tree = runner
        .text("git", &["rev-parse", "HEAD^{tree}"], &root.join("reverie"))?
        .trim()
        .to_owned();
    let gitlink = runner
        .text("git", &["rev-parse", "HEAD:reverie"], root)?
        .trim()
        .to_owned();
    require(
        gitlink == rv_head,
        "initialized Reverie differs from committed gitlink",
    )?;
    let pin = runner
        .text(
            "bash",
            &["./ci/run-reverie-pin-check.sh", "--offline", "--print-pin"],
            root,
        )?
        .trim()
        .to_owned();
    require(
        pin == rv_head,
        "all manifests/locks must select initialized exact Reverie pin",
    )?;
    let standalone = build_leaf(root, &bundle, &oracles, true, deadline)?;
    let detcore = build_leaf(root, &bundle, &oracles, false, deadline)?;
    let guest = bundle.join("m1_allocator_guest");
    let warning_guest = bundle.join("m1_allocator_guest.warnings");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let compiler_version = runner.text(&cc, &["--version"], root)?;
    // Retain the original full warning-enabled compile and link. The measured
    // guest is built separately with the shared qualifier's exact fixed recipe.
    runner.run(
        &cc,
        &[
            "-std=c11".into(),
            "-O0".into(),
            "-Wall".into(),
            "-Wextra".into(),
            "-Werror".into(),
            "-fno-builtin".into(),
            "-fno-lto".into(),
            "-Wl,--export-dynamic".into(),
            "-Wl,-z,now".into(),
            oracles["c_workload"].to_string_lossy().into_owned(),
            "-o".into(),
            warning_guest.to_string_lossy().into_owned(),
            "-ldl".into(),
        ],
        root,
    )?;
    m1_artifact::write_new(
        &bundle.join("guest-warnings.json"),
        &serde_json::to_vec_pretty(&m1_artifact::GuestReceipt {
            schema_version: 1,
            source: FileIdentity::read(&oracles["c_workload"])?,
            binary: FileIdentity::read(&warning_guest)?,
            compiler_version: compiler_version.clone(),
            command: runner
                .commands
                .last()
                .ok_or("missing warning-enabled C compile evidence")?
                .clone(),
            executed_tests: 0,
        })
        .map_err(|e| e.to_string())?,
    )?;
    runner.run(
        &cc,
        &[
            "-std=c11".into(),
            "-O0".into(),
            "-fno-builtin".into(),
            "-fno-lto".into(),
            "-Wl,--export-dynamic".into(),
            "-Wl,-z,now".into(),
            oracles["c_workload"].to_string_lossy().into_owned(),
            "-o".into(),
            guest.to_string_lossy().into_owned(),
            "-ldl".into(),
        ],
        root,
    )?;
    fs::set_permissions(&guest, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    let guest_receipt = bundle.join("guest.json");
    m1_artifact::write_new(
        &guest_receipt,
        &serde_json::to_vec_pretty(&m1_artifact::GuestReceipt {
            schema_version: 1,
            source: FileIdentity::read(&oracles["c_workload"])?,
            binary: FileIdentity::read(&guest)?,
            compiler_version,
            command: runner
                .commands
                .last()
                .ok_or("missing C compile evidence")?
                .clone(),
            executed_tests: 0,
        })
        .map_err(|e| e.to_string())?,
    )?;
    m1_artifact::verify_guest_receipt(
        &guest_receipt,
        &guest,
        &oracles["c_workload"],
        m1_artifact::FROZEN_C_ORACLE,
    )?;
    let after = source(root, &root.join("Cargo.lock"), &oracles, &mut runner)?;
    let rv_after_head = runner
        .text("git", &["rev-parse", "HEAD"], &root.join("reverie"))?
        .trim()
        .to_owned();
    let rv_after_tree = runner
        .text("git", &["rev-parse", "HEAD^{tree}"], &root.join("reverie"))?
        .trim()
        .to_owned();
    require(
        before == after && rv_head == rv_after_head && rv_tree == rv_after_tree,
        "H/RV source changed during fixture production",
    )?;
    let producer_source = root.join("ci/build-liteinst-allocator-fixtures.rs");
    let envelope = Bundle {
        schema: 1,
        hermit_head: before.head,
        hermit_tree: before.tree,
        hermit_lock_sha256: before.lock.sha256,
        nested_lock_sha256: m1_artifact::file_sha256(
            &root.join("liteinst-runtime-build/Cargo.lock"),
        )?,
        reverie_head: rv_head,
        reverie_tree: rv_tree,
        oracle_sha256: before
            .oracles
            .iter()
            .map(|(name, file)| (name.clone(), file.sha256.clone()))
            .collect(),
        guest,
        guest_receipt,
        standalone,
        detcore,
        config_fingerprint: detcore_model::config::config_wire_fingerprint(),
        producer_sha256: m1_artifact::file_sha256(&producer_source)?,
        producer_source,
        executed_tests: 0,
        full_m1_pass_claimed: false,
    };
    let manifest = bundle.join("bundle.json");
    m1_artifact::write_new(
        &manifest,
        &serde_json::to_vec_pretty(&envelope).map_err(|e| e.to_string())?,
    )?;
    // Publish only after both runtime and guest identities are closed. A failed
    // generation keeps its logs but never becomes the selected fixture bundle.
    let pending = root.join(format!("{POINTER}.{}", std::process::id()));
    m1_artifact::write_new(&pending, format!("{}\n", manifest.display()).as_bytes())?;
    fs::rename(pending, root.join(POINTER)).map_err(|e| e.to_string())?;
    println!(
        "{}",
        json!({"bundle":manifest,"executed_tests":0,"full_m1_pass_claimed":false})
    );
    Ok(())
}

fn run() -> Result<()> {
    let root = fs::canonicalize(std::env::current_dir().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => produce(&root),
        [option] if option == "--print-standalone" => {
            let manifest = fs::read_to_string(root.join(POINTER)).map_err(|e| e.to_string())?;
            let bundle: Bundle =
                serde_json::from_slice(&fs::read(manifest.trim()).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            require(
                bundle.schema == 1 && bundle.executed_tests == 0 && !bundle.full_m1_pass_claimed,
                "fixture envelope is not a producer-only receipt",
            )?;
            println!(
                "{}",
                m1_artifact::real_file(&bundle.standalone.runtime)?.display()
            );
            Ok(())
        }
        _ => Err("usage: build-liteinst-allocator-fixtures.rs [--print-standalone]".into()),
    }
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("M1 fixture producer: {error}");
            ExitCode::FAILURE
        }
    }
}
