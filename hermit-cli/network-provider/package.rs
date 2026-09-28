#!/usr/bin/env -S rust-script --force
//! Explicit, offline network-provider artifact production.
//!
//! No helper runs or BPF loads occur here. Ordinary Cargo builds do not invoke
//! this action. A failed output remains available for diagnosis; it is never
//! overwritten, called ready, or used as a live-network fallback.
//!
//! ```cargo
//! [package]
//! edition = "2024"
//! [dependencies]
//! anyhow = "=1.0.104"
//! flate2 = "=1.1.9"
//! libc = "=0.2.189"
//! serde = { version = "=1.0.228", features = ["derive"] }
//! serde_json = "=1.0.149"
//! sha2 = "=0.10.9"
//! ```

/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause
 */

mod package_support;
#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::fs::{self};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use package_support::Contract;
use package_support::MAX_ARTIFACT;
use package_support::MAX_INPUT;
use package_support::digest;
use package_support::prove_compression;
use package_support::read_regular;
use package_support::validate_elf;
use serde_json::Value;
use serde_json::json;

const NAMESPACE_SETUP: &str = "hermit-grouped-namespace-setup";
const NAMESPACE_SETUP_SOURCES: [&str; 3] = [
    "grouped-namespace-setup.c",
    "grouped-namespace-policy.c",
    "grouped-namespace-policy.h",
];

fn emit(path: &Path, value: &Value) -> Result<()> {
    let mut file = File::options().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    Ok(())
}

struct Arguments {
    component: String,
    output: PathBuf,
    source: PathBuf,
    clang: PathBuf,
    bpftool: PathBuf,
    objcopy: PathBuf,
    libbpf: PathBuf,
}

impl Arguments {
    fn parse() -> Result<Self> {
        let mut values = BTreeMap::new();
        let mut args = std::env::args_os().skip(1);
        while let Some(name) = args.next() {
            if name == "--help" {
                println!(
                    "package.rs --output-dir NEW-DIRECTORY [--component accepted|unix-guard] [--source-dir DIRECTORY] [--clang ABSOLUTE] [--bpftool ABSOLUTE] [--objcopy ABSOLUTE] [--libbpf ABSOLUTE]\nOffline compile only. Requires local clang with BPF, bpftool, llvm-objcopy, libbpf headers/library, and the exact reviewed running kernel BTF. Existing exact packages are checked; failed/stale packages are never overwritten."
                );
                std::process::exit(0);
            }
            let label = name.to_str().context("non-UTF8 option name")?.to_owned();
            ensure!(
                [
                    "--output-dir",
                    "--source-dir",
                    "--component",
                    "--clang",
                    "--bpftool",
                    "--objcopy",
                    "--libbpf"
                ]
                .contains(&label.as_str()),
                "unknown option: {label}"
            );
            let value = args
                .next()
                .with_context(|| format!("missing value for {label}"))?;
            ensure!(
                values.insert(label, value).is_none(),
                "repeated package option"
            );
        }
        let output = PathBuf::from(
            values
                .remove("--output-dir")
                .context("--output-dir is required")?,
        );
        let source = PathBuf::from(values.remove("--source-dir").unwrap_or_else(|| Path::new(file!()).parent().expect("script directory").as_os_str().to_owned())).canonicalize().context("source directory missing; provide --source-dir explicitly if invoking a separately compiled action")?;
        let component = values
            .remove("--component")
            .unwrap_or_else(|| "accepted".into())
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF8 component"))?;
        ensure!(
            component == "accepted" || component == "unix-guard",
            "unsupported component"
        );
        let mut tool = |name: &str, default: &str| {
            PathBuf::from(values.remove(name).unwrap_or_else(|| default.into()))
        };
        Ok(Self {
            component,
            output,
            source,
            clang: tool("--clang", "/usr/bin/clang"),
            bpftool: tool("--bpftool", "/usr/local/bin/bpftool"),
            objcopy: tool("--objcopy", "/usr/bin/llvm-objcopy"),
            libbpf: tool("--libbpf", "/lib64/libbpf.so.1"),
        })
    }
}

#[path = "process_group.rs"]
mod process_group;
use process_group::{Limits, OwnedChild, supervise};

fn step(
    work: &Path,
    name: &str,
    argv: &[OsString],
    deadline: Instant,
    stdout: Option<&Path>,
) -> Result<()> {
    let started = Instant::now();
    let remaining = deadline
        .checked_duration_since(started)
        .context("120-second package deadline exceeded")?
        .min(Duration::from_secs(30));
    let output_path = stdout
        .map(Path::to_owned)
        .unwrap_or_else(|| work.join(format!("{name}.stdout")));
    let output = File::options()
        .write(true)
        .create_new(true)
        .open(&output_path)?;
    let error_path = work.join(format!("{name}.stderr"));
    let error = File::options()
        .write(true)
        .create_new(true)
        .open(&error_path)?;
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(output)
        .stderr(error)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .env("LC_ALL", "C");
    // All operations below are async-signal-safe before exec. No shell,
    // inherited environment authority, BPF syscall, or helper invocation.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            for (resource, limit) in [
                (libc::RLIMIT_CORE, 0),
                (libc::RLIMIT_FSIZE, MAX_INPUT as u64),
                (libc::RLIMIT_AS, 512 * 1024 * 1024),
            ] {
                let value = libc::rlimit {
                    rlim_cur: limit,
                    rlim_max: limit,
                };
                if libc::setrlimit(resource, &value) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let argv_json = argv
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    process_group::own_descendants()?;
    let child = command.spawn();
    if let Err(error) = &child {
        emit(
            &work.join(format!("{name}.json")),
            &json!({"argv":argv_json,"spawn_error":error.to_string(),"returncode":null,"passed":false}),
        )?;
    }
    let receipt = supervise(
        OwnedChild {
            child: child.with_context(|| format!("start {name}"))?,
            cleanup_attempted: false,
            reaped: false,
        },
        started,
        &output_path,
        &error_path,
        Limits {
            wall: remaining,
            // Each file remains capped at16MiB by the unchanged RLIMIT_FSIZE.
            // Their combined readback cannot exceed the two existing files.
            logs: 2 * MAX_INPUT as u64,
            cleanup: Duration::from_secs(2),
        },
    );
    let passed = receipt["passed"] == true;
    let natural = receipt["natural_terminal_group"] == true;
    // Keep the old clean-group success prerequisite and also require actual
    // final absence. A descendant killed by cleanup cannot satisfy it.
    let clean_group = natural && receipt["final_group_absent"] == true;
    emit(
        &work.join(format!("{name}.json")),
        &json!({"argv":argv_json,"pid":receipt["pid"],"returncode":receipt["raw_status"],"signal":receipt["signal"],"timed_out":receipt["timed_out"],
            "group_absent":clean_group,"remaining_group_killed":if natural {Value::Null} else {json!(receipt["owned_group_kill_before_reap"] == true && receipt["final_group_absent"] == true)},
            "elapsed_seconds":started.elapsed().as_secs_f64(),"passed":passed,"supervision":receipt}),
    )?;
    ensure!(
        passed,
        "{name} failed; exact status/stderr retained in {}",
        work.display()
    );
    Ok(())
}

fn source_hashes(source: &Path, names: &[String]) -> Result<BTreeMap<String, String>> {
    names
        .iter()
        .map(|name| {
            Ok((
                name.clone(),
                digest(&read_regular(&source.join(name), MAX_ARTIFACT)?),
            ))
        })
        .collect()
}

fn run() -> Result<()> {
    let args = Arguments::parse()?;
    ensure!(
        cfg!(all(target_os = "linux", target_arch = "x86_64")),
        "provider packaging requires supported Linux x86_64"
    );
    for tool in [&args.clang, &args.bpftool, &args.objcopy, &args.libbpf] {
        ensure!(
            tool.is_absolute() && tool.is_file(),
            "required explicit local tool/library absent: {}",
            tool.display()
        );
    }
    let contract_name = format!("{}-contract.json", args.component);
    let contract = Contract::parse(&read_regular(&args.source.join(&contract_name), 32768)?)?;
    let accepted = args.component == "accepted";
    let grouped_build = contract.grouped_event.is_some();
    let ftrace = contract.ftrace_only;
    let grouped = grouped_build && !ftrace;
    ensure!(!grouped_build || accepted, "nonclassic topology requires accepted component");
    ensure!(!ftrace || grouped_build, "ftrace topology requires compatibility coverage");
    let mut names = contract.source_files.clone();
    names.extend([
        contract_name,
        "package.rs".to_owned(),
        "package_support.rs".to_owned(),
        "process_group.rs".to_owned(),
        // Both components compile this module and its embedded fixtures. Keep
        // these inputs in the cache identity even for the Unix-only package.
        "grouped_contract.rs".to_owned(),
        "accepted-classic-v40-contract.json".to_owned(),
        "accepted-grouped-v4-contract.json".to_owned(),
    ]);
    if grouped {
        names.extend(NAMESPACE_SETUP_SOURCES.map(str::to_owned));
    }
    let source_digests = source_hashes(&args.source, &names)?;
    let btf = read_regular(Path::new("/sys/kernel/btf/vmlinux"), MAX_INPUT)?;
    let checked_btf = contract.require_btf(&btf)?;
    let object_name = if accepted {
        "accepted-provider.bpf.o"
    } else {
        "unix-guard.bpf.o"
    };
    let library_name = if accepted {
        "libhermit_accepted_provider.so"
    } else {
        "libhermit_unix_guard_client.so"
    };
    let kind = if accepted {
        "hermit-accepted-provider"
    } else {
        "hermit-unix-guard"
    };
    let mut artifacts = BTreeMap::from([
        ("object_sha256", object_name),
        ("library_sha256", library_name),
    ]);
    if grouped {
        artifacts.insert("namespace_setup_sha256", NAMESPACE_SETUP);
    }
    if !accepted {
        artifacts.insert("helper_sha256", "hermit-unix-keeper");
        artifacts.insert("readback_sha256", "hermit-unix-readback");
    }
    let out = std::path::absolute(&args.output)?;
    match fs::symlink_metadata(&out) {
        Ok(meta) => {
            ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "package output is not a real directory"
            );
            let manifest: Value =
                serde_json::from_slice(&read_regular(&out.join("manifest.json"), 32768)?)?;
            ensure!(
                manifest["schema"] == 1
                    && manifest["kind"] == kind
                    && manifest["abi_version"] == contract.abi_version
                    && manifest["object"] == object_name
                    && manifest["library"] == library_name
                    && manifest["compile_only"] == true
                    && (!accepted || match manifest.get("copy_version") {
                        None if contract.abi_version == "4150525553540007" => contract.accepted_copy_version()? == 4,
                        Some(value) => value.as_u64() == Some(contract.accepted_copy_version()?),
                        None => false,
                    })
                    && manifest["btf_sha256"] == contract.btf_sha256
                    && manifest["sources"] == serde_json::to_value(&source_digests)?
                    && manifest["maps"] == contract.maps
                    && manifest["programs"] == contract.programs
                    && manifest["links"] == contract.links
                    && match manifest.get("ftrace_only") {
                        Some(value)=>value.as_bool()==Some(ftrace),
                        None=>!ftrace,
                    }
                    && match (
                        manifest.get("namespace_setup"),
                        manifest.get("namespace_setup_sha256"),
                        grouped,
                    ) {
                        (Some(name), Some(hash), true) =>
                            name.as_str() == Some(NAMESPACE_SETUP) && hash.as_str().is_some(),
                        (None, None, false) => true,
                        _ => false,
                    }
                    && match &contract.grouped_event {
                        Some(group) => manifest.get("grouped_event") == Some(&serde_json::to_value(group)?),
                        None => manifest.get("grouped_event").is_none(),
                    },
                "existing provider package is stale; select a fresh owned directory"
            );
            for (key, name) in &artifacts {
                ensure!(
                    manifest[*key] == digest(&read_regular(&out.join(name), MAX_ARTIFACT)?),
                    "existing artifact hash mismatch"
                );
            }
            println!("{}", json!({"package":out,"existing_exact_package":true}));
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::create_dir(&out)?;
    let work = out.join("build");
    fs::create_dir(&work)?;
    emit(&work.join("btf-contract.json"), &checked_btf)?;
    fs::write(work.join("input.btf"), &btf)?;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(120);
    let os = |value: &str| OsString::from(value);
    step(
        &work,
        "btf-header",
        &[
            args.bpftool.as_os_str().to_owned(),
            os("btf"),
            os("dump"),
            os("file"),
            work.join("input.btf").into_os_string(),
            os("format"),
            os("c"),
        ],
        deadline,
        Some(&work.join("vmlinux.h")),
    )?;
    let mut flags = vec![
        args.clang.as_os_str().to_owned(),
        os("-O2"),
        os("-g"),
        OsString::from(format!(
            "-ffile-prefix-map={}=.",
            args.source.display()
        )),
        os("-Wall"),
        os("-Wextra"),
        os("-Werror"),
    ];
    if accepted {
        flags.push(os(&format!("-DAP_NATIVE_COPY_VERSION={}ULL", contract.accepted_copy_version()?)));
    }
    if ftrace {
        flags.push(os("-DAP_FTRACE_PROVIDER=1"));
    }
    let src = if accepted {
        args.source.clone()
    } else {
        args.source.join("unix")
    };
    let mut bpf = flags.clone();
    bpf.extend([
        os("-target"),
        os("bpf"),
        os("-D__BPF__"),
        os("-fms-extensions"),
        os("-I"),
        work.as_os_str().to_owned(),
        os("-I"),
        src.as_os_str().to_owned(),
        os("-c"),
        src.join(if grouped_build {
            "provider-grouped.bpf.c"
        } else if accepted {
            "provider.bpf.c"
        } else {
            "unix-guard.bpf.c"
        })
        .into_os_string(),
        os("-o"),
        work.join("uncompressed.bpf.o").into_os_string(),
    ]);
    step(&work, "bpf", &bpf, deadline, None)?;
    // The broker and provider use the same authenticated sealed library.
    // Only this codec object receives the private digest symbol; the runtime
    // owner supplies its existing bounded SHA256 callback without libcrypto.
    let adoption = work.join("grouped-adoption-wire.o");
    if grouped {
        let mut codec = flags.clone();
        codec.extend([
            os("-fPIC"),
            os("-DSHA256=hermit_grouped_broker_sha256"),
            os("-c"),
            src.join("grouped-adoption-wire.c").into_os_string(),
            os("-o"),
            adoption.as_os_str().to_owned(),
        ]);
        step(&work, "grouped-adoption", &codec, deadline, None)?;
    }
    let mut shared = flags.clone();
    shared.extend([os("-shared"), os("-fPIC")]);
    if accepted {
        shared.extend([
            src.join(if grouped_build { "driver-grouped.c" } else { "driver.c" }).into_os_string(),
            src.join("adapter-abi.c").into_os_string(),
            args.libbpf.as_os_str().to_owned(),
        ]);
        if grouped {
            shared.extend([
                "grouped-owner.c",
                "grouped-io.c",
                "grouped-broker-bridge.c",
                "grouped-cleanup-bridge.c",
                "grouped-keeper-wire.c",
                "grouped-guardian-bootstrap.c",
                "grouped-keeper-dual.c",
            ].map(|name| src.join(name).into_os_string()));
            shared.push(adoption.into_os_string());
        }
    } else {
        shared.extend([
            src.join("keeper-channel.c").into_os_string(),
            src.join("keeper-monitor.c").into_os_string(),
        ]);
    }
    shared.extend([os("-o"), out.join(library_name).into_os_string()]);
    step(&work, "shared", &shared, deadline, None)?;
    if grouped {
        // This libc-only executable is built, never invoked, by packaging.
        // Its privilege still comes from the separately authorized launcher,
        // not from this source fingerprint or the manifest's content hash.
        let mut setup = flags.clone();
        setup.extend([os("-UNDEBUG"), os("-fPIE"), os("-pie")]);
        setup.extend(NAMESPACE_SETUP_SOURCES[..2].iter().map(|name| src.join(name).into_os_string()));
        setup.extend([os("-o"), out.join(NAMESPACE_SETUP).into_os_string()]);
        step(&work, "namespace-setup", &setup, deadline, None)?;
    }
    if !accepted {
        let mut helper = flags.clone();
        helper.extend([os("-fPIE"), os("-pie")]);
        helper.extend(
            [
                "keeper-executable.c",
                "keeper-main.c",
                "keeper-session.c",
                "keeper-channel.c",
                "keeper-monitor.c",
            ]
            .map(|name| src.join(name).into_os_string()),
        );
        helper.extend([
            args.libbpf.as_os_str().to_owned(),
            os("-o"),
            out.join("hermit-unix-keeper").into_os_string(),
        ]);
        step(&work, "keeper", &helper, deadline, None)?;
        // The separate metadata executable has no libbpf, session, load or
        // attach implementation. Only this artifact receives ID-query privilege.
        let mut readback = flags;
        readback.extend([
            os("-fPIE"),
            os("-pie"),
            os("-ffunction-sections"),
            os("-fdata-sections"),
            os("-Wl,--gc-sections"),
        ]);
        readback.extend(
            [
                "keeper-readback-main.c",
                "keeper-readback.c",
                "keeper-channel.c",
            ]
            .map(|name| src.join(name).into_os_string()),
        );
        readback.extend([os("-o"), out.join("hermit-unix-readback").into_os_string()]);
        step(&work, "readback", &readback, deadline, None)?;
    }
    step(
        &work,
        "compress-dwarf",
        &[
            args.objcopy.as_os_str().to_owned(),
            os("--compress-debug-sections=zlib"),
            os("--compress-sections=.rel.debug_*=zlib"),
            work.join("uncompressed.bpf.o").into_os_string(),
            out.join(object_name).into_os_string(),
        ],
        deadline,
        None,
    )?;
    let object = read_regular(&out.join(object_name), MAX_ARTIFACT)?;
    let library = read_regular(&out.join(library_name), MAX_ARTIFACT)?;
    let proof = prove_compression(
        &read_regular(&work.join("uncompressed.bpf.o"), MAX_INPUT)?,
        &object,
    )?;
    emit(&work.join("runtime-preservation.json"), &proof)?;
    validate_elf(&object, 247, 1)?;
    validate_elf(&library, 62, 3)?;
    ensure!(
        source_digests == source_hashes(&args.source, &names)?,
        "provider source changed during compile"
    );
    ensure!(
        started.elapsed() <= Duration::from_secs(120),
        "package aggregate deadline exceeded"
    );
    let mut manifest = json!({"schema":1,"kind":kind,"abi_version":contract.abi_version,"object":object_name,"library":library_name,"object_sha256":digest(&object),"library_sha256":digest(&library),"btf_sha256":contract.btf_sha256,"maps":contract.maps,"programs":contract.programs,"links":contract.links,"sources":source_digests,"compile_only":true,"supported_kernel_contract":"Exact reviewed BTF; fresh exact-artifact native qualification required before activation","compile_seconds":started.elapsed().as_secs_f64()});
    if ftrace {manifest["ftrace_only"]=json!(true);}
    if accepted {
        manifest["copy_version"] = json!(contract.accepted_copy_version()?);
    }
    if let Some(group) = &contract.grouped_event {
        manifest["grouped_event"] = serde_json::to_value(group)?;
    }
    if grouped {
        let setup = read_regular(&out.join(NAMESPACE_SETUP), MAX_ARTIFACT)?;
        validate_elf(&setup, 62, 3)?;
        manifest["namespace_setup"] = json!(NAMESPACE_SETUP);
        manifest["namespace_setup_sha256"] = json!(digest(&setup));
    }
    if !accepted {
        let helper = read_regular(&out.join("hermit-unix-keeper"), MAX_ARTIFACT)?;
        validate_elf(&helper, 62, 3)?;
        manifest["helper"] = json!("hermit-unix-keeper");
        manifest["helper_sha256"] = json!(digest(&helper));
        let readback = read_regular(&out.join("hermit-unix-readback"), MAX_ARTIFACT)?;
        validate_elf(&readback, 62, 3)?;
        manifest["readback"] = json!("hermit-unix-readback");
        manifest["readback_sha256"] = json!(digest(&readback));
    }
    emit(&out.join("manifest.json"), &manifest)?;
    println!(
        "{}",
        json!({"package":out,"manifest_sha256":digest(&read_regular(&out.join("manifest.json"),32768)?)})
    );
    Ok(())
}

fn main() {
    rust_script_prelude::init();
    if let Err(error) = run() {
        eprintln!("network provider package unavailable: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod compiler_supervision_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn evidence() -> PathBuf {
        let root = std::env::var_os("HERMIT_TEST_ACTION_RESULTS")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = root.join(format!(
            "package-step-control-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn actual_compiler_step_retains_leader_until_group_signal_then_reaps() {
        let path = evidence();
        step(
            &path,
            "success",
            &[
                OsString::from("/bin/sh"),
                OsString::from("-c"),
                OsString::from("exit 0"),
            ],
            Instant::now() + Duration::from_secs(30),
            None,
        )
        .unwrap();
        let receipt: Value =
            serde_json::from_slice(&fs::read(path.join("success.json")).unwrap()).unwrap();
        assert_eq!(receipt["returncode"], 0);
        assert_eq!(receipt["passed"], true);
        let owner = &receipt["supervision"];
        assert_eq!(owner["terminal_observed_without_reap"], true);
        assert_eq!(owner["owned_group_kill_before_reap"], true);
        assert_eq!(owner["unreaped_child_retained_until_receipt"], false);
        assert_eq!(owner["natural_terminal_group"], true);
        assert_eq!(owner["cleanup_complete"], true);
        assert_eq!(owner["final_group_absent"], true);
    }
    #[test]
    fn actual_compiler_nonzero_status_cannot_become_success_after_cleanup() {
        let path = evidence();
        assert!(
            step(
                &path,
                "failure",
                &[
                    OsString::from("/bin/sh"),
                    OsString::from("-c"),
                    OsString::from("exit 7")
                ],
                Instant::now() + Duration::from_secs(30),
                None
            )
            .is_err()
        );
        let receipt: Value =
            serde_json::from_slice(&fs::read(path.join("failure.json")).unwrap()).unwrap();
        assert_eq!(receipt["returncode"], 7);
        assert_eq!(receipt["passed"], false);
        assert_eq!(receipt["supervision"]["owned_group_kill_before_reap"], true);
        assert_eq!(receipt["supervision"]["cleanup_complete"], true);
        assert_eq!(receipt["supervision"]["final_group_absent"], true);
    }
}
