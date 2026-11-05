/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

fn quote(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', "'\\''"))
}

fn build(cc: &str, root: &Path, output: &Path, arguments: &[String], provenance: &mut String) {
    let command = format!(
        "cd {} && {} {} -o {}",
        quote(&root.display().to_string()),
        quote(cc),
        arguments
            .iter()
            .map(|arg| quote(arg))
            .collect::<Vec<_>>()
            .join(" "),
        quote(&output.display().to_string())
    );
    let result = Command::new(cc)
        .current_dir(root)
        .args(arguments)
        .arg("-o")
        .arg(output)
        .env("LC_ALL", "C")
        .output()
        .expect("execute C compiler");
    fs::write(output.with_extension("build.log"), &result.stderr).expect("write compiler log");
    assert!(
        result.status.success(),
        "{command}\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let hash = Command::new("sha256sum")
        .arg(output)
        .output()
        .expect("execute sha256sum");
    assert!(hash.status.success(), "sha256sum failed");
    provenance.push_str(&format!(
        "command: {command}\nsha256: {}",
        String::from_utf8_lossy(&hash.stdout)
    ));
}

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn main() {
    println!("cargo:rerun-if-env-changed=CC");
    assert_eq!(
        env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("linux"),
        "ELF loader requires Linux"
    );
    assert_eq!(
        env::var("CARGO_CFG_TARGET_ARCH").as_deref(),
        Ok("x86_64"),
        "ELF loader requires x86-64"
    );
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    let cc = env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let mut provenance = String::new();
    for command in [&cc, "rustc"] {
        let version = Command::new(command)
            .arg("--version")
            .output()
            .expect("read tool version");
        assert!(version.status.success(), "tool version failed");
        provenance.push_str(&String::from_utf8_lossy(&version.stdout));
    }
    for source in [
        "loader/loader.c",
        "loader/entry.S",
        "loader/loader.ld",
        "tests/fixtures/layout.c",
        "tests/fixtures/layout_entry.S",
        "tests/fixtures/observer.S",
        "tests/fixtures/observer_dummy.S",
        "tests/fixtures/pkey_probe.S",
        "tests/fixtures/open_trace.c",
    ] {
        println!("cargo:rerun-if-changed={source}");
        let hash = Command::new("sha256sum")
            .arg(root.join(source))
            .output()
            .expect("hash source");
        assert!(hash.status.success(), "missing source {source}");
        provenance.push_str(&format!(
            "source sha256: {}",
            String::from_utf8_lossy(&hash.stdout)
        ));
    }
    let loader = output.join("loader.elf");
    let stack_guard_control = output.join("stack-guard-control");
    build(
        &cc,
        &root,
        &stack_guard_control,
        &args(&[
            "-std=c11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fno-builtin",
            "-fno-pie",
            "-no-pie",
            "-DLOADER_STACK_GUARD_CONTROL",
            "loader/loader.c",
        ]),
        &mut provenance,
    );
    println!(
        "cargo:rustc-env=ELF_LOADER_STACK_GUARD_CONTROL={}",
        stack_guard_control.display()
    );
    let open_trace = output.join("open-trace.so");
    build(
        &cc,
        &root,
        &open_trace,
        &args(&[
            "-std=c11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fPIC",
            "-shared",
            "-fno-builtin",
            "-Wl,--build-id=none",
            "tests/fixtures/open_trace.c",
        ]),
        &mut provenance,
    );
    println!(
        "cargo:rustc-env=ELF_LOADER_OPEN_TRACE={}",
        open_trace.display()
    );
    let loader_args = args(&[
        "-std=c11",
        "-O2",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-ffreestanding",
        "-nostdlib",
        "-static",
        "-fno-pie",
        "-no-pie",
        "-fno-stack-protector",
        "-fno-builtin",
        "-fno-asynchronous-unwind-tables",
        "-fno-unwind-tables",
        "-Wl,--build-id=none",
        "-Wl,-T,loader/loader.ld",
        "loader/loader.c",
        "loader/entry.S",
    ]);
    build(&cc, &root, &loader, &loader_args, &mut provenance);
    println!("cargo:rustc-env=ELF_LOADER_TEMPLATE={}", loader.display());
    let test_loader = output.join("loader-test.elf");
    let mut test_args = loader_args;
    test_args.push("-DLOADER_TEST_MUTATIONS".to_owned());
    build(&cc, &root, &test_loader, &test_args, &mut provenance);
    println!(
        "cargo:rustc-env=ELF_LOADER_TEST_TEMPLATE={}",
        test_loader.display()
    );
    for (name, flags, variable) in [
        ("layout-pie", vec!["-fPIE", "-pie"], "ELF_LOADER_LAYOUT_PIE"),
        (
            "layout-nonpie",
            vec!["-fno-pie", "-no-pie"],
            "ELF_LOADER_LAYOUT_NONPIE",
        ),
        (
            "layout-low",
            vec!["-fno-pie", "-no-pie", "-Wl,-Ttext-segment=0x3ff000"],
            "ELF_LOADER_LOW_TARGET",
        ),
    ] {
        let path = output.join(name);
        let mut arguments = args(&[
            "-O1",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fno-stack-protector",
            "-Wl,--build-id=none",
            "-Wl,-e,fixture_start",
            "tests/fixtures/layout.c",
            "tests/fixtures/layout_entry.S",
        ]);
        arguments.extend(args(&flags));
        build(&cc, &root, &path, &arguments, &mut provenance);
        println!("cargo:rustc-env={variable}={}", path.display());
    }
    let observer = output.join("observer.elf");
    build(
        &cc,
        &root,
        &observer,
        &args(&[
            "-nostdlib",
            "-static-pie",
            "-Wl,--build-id=none",
            "tests/fixtures/observer.S",
        ]),
        &mut provenance,
    );
    println!("cargo:rustc-env=ELF_LOADER_OBSERVER={}", observer.display());
    for (name, interpreter, variable) in [
        ("pkey-probe-main", false, "ELF_LOADER_PKEY_PROBE_MAIN"),
        (
            "pkey-probe-interpreter",
            true,
            "ELF_LOADER_PKEY_PROBE_INTERPRETER",
        ),
    ] {
        let path = output.join(name);
        let mut arguments = args(&[
            "-nostdlib",
            "-static-pie",
            "-Wl,--build-id=none",
            "tests/fixtures/pkey_probe.S",
        ]);
        if interpreter {
            arguments.push("-DPKEY_PROBE_INTERPRETER".to_owned());
        }
        build(&cc, &root, &path, &arguments, &mut provenance);
        println!("cargo:rustc-env={variable}={}", path.display());
    }
    for (name, flags, variable) in [
        ("entry-pie", vec!["-fPIE", "-pie"], "ELF_LOADER_ENTRY_PIE"),
        (
            "entry-nonpie",
            vec!["-fPIE", "-pie", "-Wl,-Ttext-segment=0x400000"],
            "ELF_LOADER_ENTRY_NONPIE",
        ),
    ] {
        let path = output.join(name);
        let mut arguments = args(&[
            "-nostdlib",
            "-Wl,--build-id=none",
            "tests/fixtures/observer_dummy.S",
        ]);
        arguments.extend(args(&flags));
        arguments.push(format!("-Wl,--dynamic-linker={}", observer.display()));
        build(&cc, &root, &path, &arguments, &mut provenance);
        println!("cargo:rustc-env={variable}={}", path.display());
    }
    fs::write(output.join("PROVENANCE.txt"), provenance).expect("write generated provenance");
    println!(
        "cargo:rustc-env=ELF_LOADER_ARTIFACT_DIR={}",
        output.display()
    );
    println!(
        "cargo:rustc-env=ELF_LOADER_PROVENANCE={}",
        output.join("PROVENANCE.txt").display()
    );
}
