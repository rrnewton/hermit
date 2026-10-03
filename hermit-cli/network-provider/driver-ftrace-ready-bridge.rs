/* SPDX-License-Identifier: BSD-3-Clause */
// Ordinary Detcore component test: compile the actual C driver under the facade,
// fence its whole object before link/exec, and validate the returned inventories.
// The rows are modeled libbpf premises, never loaded-kernel or READY receipts.
use std::fs;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use driver_ftrace_process::DRIVER_FTRACE_IMPORT_FENCE;
use driver_ftrace_process::LIMITS;
use driver_ftrace_process::execute_stage;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;

use super::super::ProviderTopology;
use super::super::ProviderWireFormat;
// Both C bridges share one maintained supervisor and its eight existing tests.
use super::super::{driver_ftrace_inputs, driver_ftrace_process, process_group};
use super::ProviderArtifact;
use super::ProviderReady;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Export {
    schema: u32,
    fixture_only: bool,
    provider_incarnation: u64,
    cases: Vec<Inventory>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    name: String,
    open_result: i32,
    open_errno: i32,
    inventory_result: i32,
    inventory_errno: i32,
    capacity: u32,
    written: u32,
    ids: Vec<Id>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Id { kind: u32, id: u32 }

fn bounded_read(path: &Path, limit: u64) -> Vec<u8> {
    let file = File::options().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path).expect("required original C inventory/source");
    let stat = file.metadata().unwrap();
    assert!(stat.is_file() && stat.len() <= limit);
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).unwrap();
    assert!(bytes.len() as u64 <= limit);
    bytes
}
fn check_compiled_sources(source: &Path) {
    for (name, compiled) in driver_ftrace_inputs::C_INPUTS.iter()
        .chain(driver_ftrace_inputs::INPUTS)
    {
        assert_eq!(bounded_read(&source.join(name), 1024 * 1024), *compiled,
            "bridge binary is stale: {name}");
    }
}

fn stage(command: &mut Command, name: &str, started: Instant, stdout: &Path, stderr: &Path) {
    let receipt = execute_stage(command, started, stdout, stderr);
    eprintln!("driver-ftrace unit stage {name}: {receipt}");
    if receipt["passed"] != true {
        for path in [stdout, stderr] {
            let mut bytes = Vec::new();
            let read = File::open(path).and_then(|file| file.take(8192).read_to_end(&mut bytes));
            eprintln!("{} excerpt ({read:?}): {}", path.display(), String::from_utf8_lossy(&bytes));
        }
        panic!("driver-ftrace unit stage {name} failed: {receipt}");
    }
}

// No prepared executable, environment input, default skip, or package receipt
// supplies this test's positive. All four child stages share one original bound.
#[test]
fn actual_ftrace_driver_inventory_uses_original_ready_validator() {
    let started = Instant::now();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../hermit-cli/network-provider").canonicalize().unwrap();
    check_compiled_sources(&source);
    assert_eq!(driver_ftrace_inputs::C_INPUTS.len(), 29);
    let scratch = tempfile::Builder::new().prefix("hermit-driver-ftrace-").tempdir().unwrap();
    let inputs = scratch.path().join("sources");
    fs::create_dir(&inputs).unwrap();
    // Compile the bytes bound into this Cargo test, including the actual driver
    // and its complete local include closure. A new unsnapshotted local header
    // fails compilation rather than silently escaping the source identity.
    for (name, bytes) in driver_ftrace_inputs::C_INPUTS {
        fs::write(inputs.join(name), bytes).unwrap();
    }
    let stdout = scratch.path().join("stdout");
    let stderr = scratch.path().join("stderr");
    File::create(&stdout).unwrap();
    File::create(&stderr).unwrap();
    let object = scratch.path().join("driver-ftrace-test.o");
    let executable = scratch.path().join("driver-ftrace-test");
    let mut compile = Command::new("clang");
    compile.args(["-std=gnu11", "-fno-builtin", "-DAP_FTRACE_PROVIDER=1",
        "-DAP_NATIVE_COPY_VERSION=5ULL", "-O2", "-Wall", "-Wextra", "-Werror",
        "-UNDEBUG", "-c"])
        .arg("-I").arg(&inputs).arg(inputs.join("driver-ftrace-test.c"))
        .arg("-o").arg(&object).current_dir(scratch.path());
    stage(&mut compile, "compile", started, &stdout, &stderr);
    let incomplete=scratch.path().join("missing-fault-header");
    fs::create_dir(&incomplete).unwrap();
    for (name,bytes) in driver_ftrace_inputs::C_INPUTS {
        if *name!="stream-copy-fault.h" {fs::write(incomplete.join(name),bytes).unwrap();}
    }
    let missing=driver_ftrace_process::execute_stage(
        Command::new("clang").args(["-std=gnu11","-fno-builtin","-DAP_FTRACE_PROVIDER=1",
            "-DAP_NATIVE_COPY_VERSION=5ULL","-O2","-Wall","-Wextra","-Werror","-UNDEBUG","-c"])
            .arg("-I").arg(&incomplete).arg(incomplete.join("driver-ftrace-test.c"))
            .arg("-o").arg(scratch.path().join("missing.o")),
        started,&stdout,&stderr);
    assert_eq!(missing["raw_status"],1,"missing header must be a compiler failure: {missing}");
    assert_eq!(missing["passed"],false);
    assert_eq!(missing["timed_out"],false);
    assert_eq!(missing["log_overflow"],false);
    assert_eq!(missing["primary_error"],serde_json::Value::Null);
    assert_eq!(missing["terminal_bounds_error"],serde_json::Value::Null);
    assert_eq!(missing["cleanup_complete"],true);
    assert_eq!(missing["final_group_absent"],true);
    assert_eq!(missing["cleanup_errors"],serde_json::json!([]));
    assert!(String::from_utf8_lossy(&bounded_read(&stderr,LIMITS.logs))
        .contains("'stream-copy-fault.h' file not found"));
    // Each new production include must belong to this closed source set. Keep
    // the older missing-fault-header negative above and the same original clock.
    for header in ["owned-metadata.h", "owned-metadata-driver.h",
        "stream-tx.h", "stream-tx-driver.h"] {
        let incomplete = scratch.path().join(format!("missing-{header}"));
        fs::create_dir(&incomplete).unwrap();
        for (name, bytes) in driver_ftrace_inputs::C_INPUTS {
            if *name != header {
                fs::write(incomplete.join(name), bytes).unwrap();
            }
        }
        let missing = driver_ftrace_process::execute_stage(
            Command::new("clang").args(["-std=gnu11", "-fno-builtin", "-DAP_FTRACE_PROVIDER=1",
                "-DAP_NATIVE_COPY_VERSION=5ULL", "-O2", "-Wall", "-Wextra", "-Werror", "-UNDEBUG", "-c"])
                .arg("-I").arg(&incomplete).arg(incomplete.join("driver-ftrace-test.c"))
                .arg("-o").arg(incomplete.join("missing.o")),
            started, &stdout, &stderr);
        assert_eq!(missing["raw_status"], 1, "missing {header} must be a compiler failure: {missing}");
        assert_eq!(missing["passed"], false);
        assert_eq!(missing["timed_out"], false);
        assert_eq!(missing["log_overflow"], false);
        assert_eq!(missing["primary_error"], serde_json::Value::Null);
        assert_eq!(missing["terminal_bounds_error"], serde_json::Value::Null);
        assert_eq!(missing["cleanup_complete"], true);
        assert_eq!(missing["final_group_absent"], true);
        assert_eq!(missing["cleanup_errors"], serde_json::json!([]));
        assert!(String::from_utf8_lossy(&bounded_read(&stderr, LIMITS.logs))
            .contains(&format!("'{header}' file not found")));
    }
    let mut fence = Command::new("python3");
    fence.arg("-c").arg(DRIVER_FTRACE_IMPORT_FENCE).arg(&object);
    stage(&mut fence, "closed-import-fence", started, &stdout, &stderr);
    let mut link = Command::new("clang");
    link.arg(&object).arg("-Wl,--no-undefined").arg("-o").arg(&executable);
    stage(&mut link, "link", started, &stdout, &stderr);
    let prefix = bounded_read(&stdout, LIMITS.logs);
    stage(Command::new(&executable).arg("inventory-export"), "inventory-export",
        started, &stdout, &stderr);
    let output = bounded_read(&stdout, LIMITS.logs);
    assert!(output.starts_with(&prefix), "earlier stage output changed");
    let raw = &output[prefix.len()..];
    assert!(raw.len() <= 65536, "original inventory export exceeds bound");
    let exported: Export = serde_json::from_slice(raw).unwrap();
    assert_eq!(exported.schema, 1);
    assert!(exported.fixture_only);
    assert_eq!(exported.provider_incarnation, 0x123456789);
    assert_eq!(exported.cases.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["good", "missing-map", "duplicate-map", "extra-map"]);

    let contract: serde_json::Value =
        serde_json::from_slice(include_bytes!("accepted-contract.json")).unwrap();
    assert_eq!(contract["maps"], 24);
    assert_eq!(contract["programs"], 49);
    assert_eq!(contract["links"], 49);
    assert_eq!(contract["shared_links"], serde_json::json!([]));
    assert_eq!(contract["ftrace_only"], true);
    let contract_sha256: [u8; 32] =
        Sha256::digest(serde_json::to_vec(&contract).unwrap()).into();
    // Nonzero artifact bytes are explicit component-fixture premises, NOT
    // measurements of a BPF load. Counts and grammar are the actual contract.
    let artifact = ProviderArtifact {
        topology: ProviderTopology::FtraceV1 { contract_sha256 },
        wire_format: ProviderWireFormat::from_package(
            contract["abi_version"].as_str().unwrap(), contract["copy_version"].as_u64()
        ).unwrap(),
        object_sha256: Sha256::digest(b"modeled ftrace BPF object only").into(),
        library_sha256: Sha256::digest(b"modeled actual-driver facade only").into(),
        btf_sha256: Sha256::digest(b"modeled target BTF only").into(),
        maps: 24, programs: 49, links: 49,
    };
    assert_eq!(artifact.inventory_capacity().unwrap().get(), 122);
    let mut run = [0u8; 16];
    run[..8].copy_from_slice(&exported.provider_incarnation.to_le_bytes());
    let mut accepted = 0;
    let mut outcomes = Vec::new();
    for case in &exported.cases {
        assert_eq!(case.open_result, 0, "original open: {}", case.name);
        // errno after successful ap_open may retain its modeled anchor EBADF;
        // only the original failed return makes errno an error result.
        assert!(case.open_errno >= 0);
        assert_eq!(case.capacity, 122);
        assert_eq!(case.ids.len(), case.written as usize);
        let mut ready = ProviderReady {
            incarnation: run, provider_incarnation: exported.provider_incarnation,
            artifact: artifact.clone(), maps: Vec::new(), programs: Vec::new(), links: Vec::new(),
        };
        for row in &case.ids {
            match row.kind {
                0 => ready.maps.push(row.id),
                1 => ready.programs.push(row.id),
                2 => ready.links.push(row.id),
                _ => panic!("unknown original C identifier kind"),
            }
        }
        // This is the existing product verifier, including its exact counts,
        // nonzero/unique IDs, artifact and incarnation rules. Do not recreate it.
        let validated = ready.validate(run, &artifact);
        if case.name == "good" {
            // Target every new object identity, not merely total population.
            for (kind, index) in [(0,23),(1,47),(1,48),(2,47),(2,48)] {
                for mutation in 0..3 {
                    let mut wrong = ready.clone();
                    let ids = match kind {0 => &mut wrong.maps, 1 => &mut wrong.programs,
                        _ => &mut wrong.links};
                    match mutation {
                        0 => { ids.remove(index); }
                        1 => ids[index]=ids[0],
                        _ => ids[index]=0,
                    }
                    assert!(wrong.validate(run, &artifact).is_err(), "new object {kind}:{index} mutation {mutation}");
                }
            }
            let mut historical = ready.clone();
            historical.maps.truncate(23); historical.programs.truncate(47);
            historical.links.truncate(47);
            assert!(historical.validate(run, &artifact).is_err());
        }
        let accepted_case = case.inventory_result == 0 && validated.is_ok();
        // Evaluate every original inventory before the outcome assertion: a
        // permissive map-count mutant must expose BOTH 23-map cases, not stop
        // after missing-map and accidentally mask duplicate-map sensitivity.
        outcomes.push((case.name.as_str(), case.inventory_result, validated.is_ok(), accepted_case));
        accepted += usize::from(accepted_case);
        if case.name == "extra-map" {
            assert_eq!(case.inventory_result, -1);
            assert_eq!(case.inventory_errno, libc::EOVERFLOW);
            assert_eq!(case.written, 122);
            assert_eq!((ready.maps.len(), ready.programs.len(), ready.links.len()), (25, 49, 48));
        } else {
            assert_eq!(case.inventory_result, 0);
            assert_eq!(case.inventory_errno, 0);
            let maps = if case.name == "good" {24} else {23};
            assert_eq!((ready.maps.len(), ready.programs.len(), ready.links.len()), (maps, 49, 49));
            assert_eq!(case.written as usize, maps + 49 + 49);
        }
    }
    eprintln!("actual ProviderReady inventory outcomes: {outcomes:?}");
    assert_eq!(outcomes, [
        ("good", 0, true, true),
        ("missing-map", 0, false, false),
        ("duplicate-map", 0, false, false),
        ("extra-map", -1, false, false),
    ], "actual ProviderReady outcomes for every original C inventory");
    assert_eq!(accepted, 1);
    assert_eq!(bounded_read(&stdout, LIMITS.logs), output, "original export changed");
    check_compiled_sources(&source);
    assert_eq!(process_group::bounds(started, &stdout, &stderr, LIMITS).unwrap(), (false, false));
    println!("\nDRIVER_FTRACE_READY_BRIDGE_V1 sha256={:x} cases=4 accepted=1 refused=3",
        Sha256::digest(raw));
}
