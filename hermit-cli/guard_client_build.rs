/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause
 */

//! Build only the small libc/Linux-UAPI guard client. BPF and the privileged
//! keeper are explicit packaged artifacts, never Cargo build-script side effects.

use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

fn checked(command: &mut Command) {
    let description = format!("{command:?}");
    let status = command.status().unwrap_or_else(|error| {
        panic!("could not start Unix guard client compiler: {description}: {error}")
    });
    assert!(
        status.success(),
        "Unix guard client build failed: {description}: {status}"
    );
}

/// Called by the existing hermit-cli build.rs without replacing its metadata or
/// Reverie-pin logic. No generated Cargo manifest or new Rust build crate needed.
pub fn build() {
    let target = env::var("TARGET").expect("Cargo TARGET");
    let host = env::var("HOST").expect("Cargo HOST");
    assert!(
        target.starts_with("x86_64-") && target.contains("-linux-"),
        "Unix guard client currently requires a Linux x86_64 target; no silent architecture fallback"
    );
    let source = Path::new("network-provider/unix");
    for name in [
        "keeper-channel.c",
        "keeper-monitor.c",
        "keeper-channel.h",
        "keeper-monitor.h",
        "keeper-session.h",
        "unix-guard.h",
    ] {
        println!("cargo:rerun-if-changed={}", source.join(name).display());
    }
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=AR");
    let compiler = |name: &str, native_default: &str| match env::var_os(name) {
        Some(value) if !value.is_empty() => value,
        _ if target == host => native_default.into(),
        _ => panic!(
            "cross-compiling Unix guard client for {target} requires an explicit {name} executable; host tools are not selected implicitly"
        ),
    };
    let cc = compiler("CC", "cc");
    let ar = compiler("AR", "ar");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let mut objects = Vec::new();
    for stem in ["keeper-channel", "keeper-monitor"] {
        let object = output.join(format!("hermit-{stem}.o"));
        checked(
            Command::new(&cc)
                .args([
                    "-std=c11", "-O2", "-fPIC", "-Wall", "-Wextra", "-Werror", "-c",
                ])
                .arg(source.join(format!("{stem}.c")))
                .arg("-o")
                .arg(&object),
        );
        objects.push(object);
    }
    let archive = output.join("libhermit_unix_guard_client.a");
    // Recreate only this exact generated output: ar's replacement mode could
    // otherwise retain an obsolete member from an older recipe.
    match std::fs::remove_file(&archive) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("cannot replace exact generated guard archive: {error}"),
    }
    checked(Command::new(&ar).arg("crsD").arg(&archive).args(&objects));
    println!("cargo:rustc-link-search=native={}", output.display());
    println!("cargo:rustc-link-lib=static=hermit_unix_guard_client");
}
