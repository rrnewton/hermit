/* Copyright (c) Meta Platforms, Inc. and affiliates. */
//! Build the maintained fixture only when this dev-only crate is selected.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let target = env::var("TARGET").expect("Cargo TARGET");
    let host = env::var("HOST").expect("Cargo HOST");
    assert!(
        target.starts_with("x86_64-") && target.contains("-linux-"),
        "CLI refusal fixture requires a Linux x86_64 target"
    );
    let compiler = match env::var_os("CC") {
        Some(value) if !value.is_empty() => value,
        _ if target == host => "cc".into(),
        _ => panic!("cross-compiling CLI refusal fixture requires an explicit CC executable"),
    };
    let source = "../../hermit-cli/tests/fixtures/accepted_startup_stderr_exec.c";
    println!("cargo:rerun-if-changed={source}");
    println!("cargo:rerun-if-env-changed=CC");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"))
        .join("accepted-startup-stderr-exec");
    let status = Command::new(compiler)
        .args([
            "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", source, "-o",
        ])
        .arg(&output)
        .status()
        .expect("start CLI refusal fixture compiler");
    assert!(status.success(), "CLI refusal fixture compilation failed");
    println!(
        "cargo:rustc-env=HERMIT_STARTUP_STDERR_FIXTURE={}",
        output.display()
    );
}
