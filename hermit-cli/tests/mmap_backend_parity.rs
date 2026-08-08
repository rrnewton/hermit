/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(feature = "dbi")]
use std::fs;
#[cfg(feature = "dbi")]
use std::path::Path;
#[cfg(feature = "dbi")]
use std::process::Command;
#[cfg(feature = "dbi")]
use std::sync::OnceLock;

const EXPECTED_COUNT: usize = 4;
#[cfg(feature = "dbi")]
static GUEST: OnceLock<std::path::PathBuf> = OnceLock::new();

#[cfg(feature = "dbi")]
fn guest() -> &'static Path {
    GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let build_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mmap-backend-parity");
        fs::create_dir_all(&build_dir).unwrap();
        let output = build_dir.join("mmap_layout_pointer_order");
        let status = Command::new("cc")
            .args(["-O1", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/backend-parity/fixtures/mmap_layout_pointer_order.c"))
            .arg("-o")
            .arg(&output)
            .status()
            .expect("failed to compile mmap parity guest");
        assert!(status.success(), "mmap parity guest compilation failed");
        output
    })
}

#[cfg(feature = "dbi")]
fn run_backend(backend: &str, perturb_third: bool) -> Vec<usize> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "run",
            "--backend",
            backend,
            "--strict",
            "--base-env=minimal",
            "--max-timeslice=disabled",
            "--",
        ])
        .arg(guest());
    if perturb_third {
        command.arg("perturb-third");
    }
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {backend}: {error}"));
    assert!(
        output.status.success(),
        "{backend} failed: status={}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_addresses(&output.stdout)
}

#[cfg(feature = "dbi")]
fn parse_addresses(stdout: &[u8]) -> Vec<usize> {
    let line = std::str::from_utf8(stdout).expect("guest stdout was not UTF-8");
    let mut fields = line.split_whitespace();
    assert_eq!(fields.next(), Some("mmap-addresses"));
    let declared_count = fields
        .next()
        .and_then(|field| field.strip_prefix("count="))
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let addresses = fields
        .map(|field| usize::from_str_radix(field.strip_prefix("0x").unwrap(), 16).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(addresses.len(), declared_count);
    addresses
}

fn compare_addresses(left: &[usize], right: &[usize]) -> Result<usize, String> {
    if left.len() != right.len() {
        return Err(format!(
            "address-count mismatch: {} != {}",
            left.len(),
            right.len()
        ));
    }
    let total = left.len();
    let matched = left
        .iter()
        .zip(right)
        .filter(|(left, right)| left == right)
        .count();
    if let Some((index, (left, right))) = left
        .iter()
        .zip(right)
        .enumerate()
        .find(|(_, (left, right))| left != right)
    {
        return Err(format!(
            "exact mmap address parity: matched {matched}/{}; mmap address {index} differs: {left:#x} != {right:#x}",
            total
        ));
    }
    Ok(left.len())
}

#[test]
#[cfg(feature = "dbi")]
fn ptrace_and_dbi_return_the_same_exact_mmap_addresses() {
    let ptrace = run_backend("ptrace", false);
    let dbi = run_backend("dbi", false);
    assert_eq!(ptrace.len(), EXPECTED_COUNT);
    let matched = compare_addresses(&ptrace, &dbi).unwrap();
    let addresses = ptrace
        .iter()
        .map(|address| format!("{address:#x}"))
        .collect::<Vec<_>>()
        .join(",");
    eprintln!(
        "exact mmap address parity: matched {matched}/{EXPECTED_COUNT}; addresses=[{addresses}]"
    );
}

#[test]
#[cfg(feature = "dbi")]
fn mmap_address_comparator_rejects_a_one_backend_allocator_perturbation() {
    let ptrace = run_backend("ptrace", false);
    let perturbed_dbi = run_backend("dbi", true);
    let mismatch = compare_addresses(&ptrace, &perturbed_dbi)
        .expect_err("one-sided DBI allocator perturbation must fail the exact comparator");
    assert!(mismatch.contains("matched 3/4"), "{mismatch}");
    assert!(mismatch.contains("address 2 differs"), "{mismatch}");
    eprintln!("negative control: {mismatch}");
}

#[test]
fn exact_comparator_logic_is_covered_without_optional_backends() {
    let baseline = [0x1000, 0x2000, 0x3000, 0x4000];
    assert_eq!(compare_addresses(&baseline, &baseline), Ok(EXPECTED_COUNT));
    let perturbed = [0x1000, 0x2000, 0x5000, 0x4000];
    let mismatch = compare_addresses(&baseline, &perturbed).unwrap_err();
    assert!(mismatch.contains("matched 3/4"), "{mismatch}");
    assert!(mismatch.contains("address 2 differs"), "{mismatch}");
}
