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
use std::os::unix::fs::MetadataExt;
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

fn create_new_package_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("package output has no parent directory")?;
    fs::create_dir_all(parent).context("create package output parent directories")?;
    fs::create_dir(path).context("create new package output directory")?;
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
    ensure!(
        work.is_absolute(),
        "package step work directory must be absolute"
    );
    let metadata = fs::symlink_metadata(work).context("stat package step work directory")?;
    ensure!(
        metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() },
        "package step work directory must be a real owned directory"
    );
    // Resolve the step's existing output directory, never ambient TMPDIR. The
    // caller owns this newly created package build directory and its lifetime.
    let scratch = work
        .canonicalize()
        .context("resolve package step work directory")?;
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
        .env("TMPDIR", &scratch)
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
                let mut inherited = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(resource, &mut inherited) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // A configured cap must never lift a stricter inherited limit.
                let value = libc::rlimit {
                    rlim_cur: inherited.rlim_cur.min(limit),
                    rlim_max: inherited.rlim_max.min(limit),
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
    ensure!(args.component != "accepted" || contract.abi_version == "415052555354000b",
        "current accepted producer requires blocking-TX ABI10; historical packages keep their original sources");
    let accepted = args.component == "accepted";
    let tx_config = if accepted { Some(package_support::blocking_tx_config()?) } else { None };
    let grouped_build = contract.grouped_event.is_some();
    let ftrace = contract.ftrace_only;
    let grouped = grouped_build && !ftrace;
    ensure!(!grouped_build || accepted, "nonclassic topology requires accepted component");
    ensure!(!ftrace || grouped_build, "ftrace topology requires compatibility coverage");
    let mut names = contract.source_files.clone();
    if accepted {
        names.extend(["owned-metadata.h", "owned-metadata-driver.h",
            "stream-copy-fault.h", "stream-tx-image.h", "stream-tx-live-image.h"].map(str::to_owned));
    }
    // Maintained topology contracts move to the current adapter together;
    // decoding historical packages remains a separate compatibility path.
    // Bind these shared bodies even for the separately named classic topology.
    if accepted && !ftrace {
        names.extend(["stream-copy-custody.inc", "stream-copy-problem.inc",
            "stream-copy-unit-enter.inc", "stream-copy-emit.inc",
            "stream-copy-unit-exit.inc", "stream-tx.h", "stream-tx.bpf.h",
            "stream-tx-driver.h"].map(str::to_owned));
    }
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
                    && manifest.get("blocking_tx_config") == tx_config.as_ref()
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
    create_new_package_directory(&out)?;
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
        // Keep generated-header and compilation-directory DWARF independent
        // of the package output path, without stripping debug or BTF sections.
        os("-fdebug-compilation-dir=."),
        OsString::from(format!("-ffile-prefix-map={}=./build", work.display())),
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
    ensure!(!accepted || tx_config.as_ref() == Some(&package_support::blocking_tx_config()?),
        "blocking-TX embedded config changed during compile");
    let mut manifest = json!({"schema":1,"kind":kind,"abi_version":contract.abi_version,"object":object_name,"library":library_name,"object_sha256":digest(&object),"library_sha256":digest(&library),"btf_sha256":contract.btf_sha256,"maps":contract.maps,"programs":contract.programs,"links":contract.links,"sources":source_digests,"compile_only":true,"supported_kernel_contract":"Exact reviewed BTF; fresh exact-artifact native qualification required before activation","compile_seconds":started.elapsed().as_secs_f64()});
    if ftrace {manifest["ftrace_only"]=json!(true);}
    if let Some(tx_config) = tx_config { manifest["blocking_tx_config"] = tx_config; }
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
    fn cold_package_output_creates_missing_parent_directories() {
        let root = evidence();
        let output = root.join("target/triple/debug/network-provider/accepted");
        create_new_package_directory(&output).unwrap();
        assert!(output.is_dir());
        assert!(create_new_package_directory(&output).is_err());
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

    // The regression parent never changes its own limits. A fresh custodian
    // owns the isolated copy invoking the real step and any observer adopted
    // after that copy dies. Maintained step/supervision receipts and the exact
    // kernel wait set remain separate; proc numbers alone grant no authority.
    fn actual_limits() -> [[libc::rlim_t; 2]; 3] {
        [libc::RLIMIT_FSIZE, libc::RLIMIT_AS, libc::RLIMIT_CORE].map(|resource| {
            let mut value = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            assert_eq!(unsafe { libc::getrlimit(resource, &mut value) }, 0);
            assert!(value.rlim_cur <= value.rlim_max);
            [value.rlim_cur, value.rlim_max]
        })
    }

    #[test]
    fn actual_package_step_limit_observer() {
        // This is also an independently counted, always-active observation
        // case in the full suite, not an ignored or environment-gated test.
        let observed_limits = actual_limits();
        let args = std::env::args().collect::<Vec<_>>();
        let isolated = args.windows(2).any(|pair| {
            pair == [
                "--exact",
                "compiler_supervision_tests::actual_package_step_limit_observer",
            ]
        });
        if isolated {
            // Capture the real entry limits before lowering only this leaf's
            // output allowance. Do not mutate limits of the shared full suite.
            limit_observer_output_cap().unwrap();
        }
        limit_observer_barrier().unwrap();
        println!(
            "\nPACKAGE-LIMIT-OBSERVATION {}",
            json!({
                "pid":std::process::id(),
                "parent":unsafe { libc::getppid() },
                "limits":observed_limits,
                "limits_phase":"entry_before_output_cap",
                "isolated_output_cap":if isolated { json!(4096) } else { Value::Null }
            })
        );
    }

    fn retain_limit_parent_record(record: &Value) {
        // Write through the real pipe rather than libtest's successful-test
        // print capture. The qualification runner retains and charges every
        // byte, including each complete original owner's supervision receipt.
        let mut output = std::io::stdout().lock();
        output.write_all(b"\nPACKAGE-LIMIT-PARENT ").unwrap();
        serde_json::to_writer(&mut output, record).unwrap();
        output.write_all(b"\n").unwrap();
        output.flush().unwrap();
    }

    fn limit_observer_output_cap() -> Result<()> {
        let mut limits = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        ensure!(unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut limits) } == 0);
        // Any libtest prefix already written must also fit the same ceiling.
        for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
            ensure!(unsafe { libc::fstat(fd, &mut status) } == 0);
            if status.st_mode & libc::S_IFMT == libc::S_IFREG {
                ensure!(
                    (0..=4096).contains(&status.st_size),
                    "observer prefix exceeds output cap"
                );
            }
        }
        limits.rlim_cur = limits.rlim_cur.min(4096);
        ensure!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &limits) } == 0);
        Ok(())
    }

    fn emit_limit_custodian_receipt(path: &Path, value: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        if bytes.len() > 2048 {
            // Preserve the complete failed outcome in the bounded action's
            // captured stream, rather than publishing a truncated JSON file.
            eprintln!("oversized custodian receipt: {value}");
            anyhow::bail!("custodian receipt exceeds2048 bytes");
        }
        let mut file = File::options().write(true).create_new(true).open(path)?;
        file.write_all(&bytes)?;
        Ok(())
    }

    // Only the fresh, exact-selector custodian may use the child-set routines
    // below. They must never sweep children of the shared libtest process.
    fn limit_clock_ns() -> Result<u64> {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        ensure!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) } == 0,
            "read fixture monotonic clock"
        );
        ensure!(value.tv_sec >= 0 && (0..1_000_000_000).contains(&value.tv_nsec));
        (value.tv_sec as u64)
            .checked_mul(1_000_000_000)
            .and_then(|seconds| seconds.checked_add(value.tv_nsec as u64))
            .context("fixture clock overflow")
    }

    fn limit_deadline(reserve_seconds: u64) -> Result<Instant> {
        let end = std::env::var("HERMIT_PACKAGE_LIMIT_DEADLINE")?.parse::<u64>()?;
        let observed = Instant::now();
        let now = limit_clock_ns()?;
        let remaining = end
            .checked_sub(reserve_seconds * 1_000_000_000)
            .and_then(|end| end.checked_sub(now))
            .context("original fixture deadline exhausted")?;
        // Snapshot Instant before CLOCK_MONOTONIC: a descheduling gap makes
        // this translation conservative, never a refreshed nested budget.
        Ok(observed + Duration::from_nanos(remaining))
    }

    fn limit_poll(fd: libc::c_int, deadline: Instant) -> Result<()> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("original fixture poll deadline")?;
        let timeout = i32::try_from(remaining.as_millis().max(1))?;
        let mut item = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut item, 1, timeout) };
        ensure!(
            result >= 0,
            "fixture poll: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            result == 1 && Instant::now() < deadline,
            "fixture poll deadline"
        );
        ensure!(
            item.revents & (libc::POLLERR | libc::POLLNVAL) == 0,
            "fixture poll error"
        );
        ensure!(
            item.revents & (libc::POLLIN | libc::POLLHUP) != 0,
            "fixture poll without readiness"
        );
        Ok(())
    }

    fn limit_pidfd(pid: u32) -> Result<std::os::fd::OwnedFd> {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        ensure!(
            fd >= 0,
            "open original child pidfd: {}",
            std::io::Error::last_os_error()
        );
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) })
    }

    fn limit_kill(fd: &std::os::fd::OwnedFd) -> Result<()> {
        use std::os::fd::AsRawFd;
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        ensure!(
            result == 0,
            "signal original pidfd: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    fn limit_childless() -> Result<bool> {
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(false); // Includes a live child with si_pid == 0.
        }
        let error = std::io::Error::last_os_error();
        ensure!(
            error.raw_os_error() == Some(libc::ECHILD),
            "child-set observation: {error}"
        );
        Ok(true)
    }

    fn limit_retire_child(pid: u32, deadline: Instant) -> Result<Value> {
        use std::os::fd::AsRawFd;
        // The caller just authenticated this unreaped child with P_PID. This
        // fresh process has one exclusive waiter; its PID cannot be recycled
        // between that observation and opening the generation-bound pidfd.
        let fd = limit_pidfd(pid)?;
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let peek = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                fd.as_raw_fd() as u32,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        ensure!(
            peek == 0,
            "pidfd wait ownership: {}",
            std::io::Error::last_os_error()
        );
        let signal_sent = unsafe { info.si_pid() } == 0;
        if signal_sent {
            limit_kill(&fd)?;
            limit_poll(fd.as_raw_fd(), deadline)?;
        }
        let observed = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                fd.as_raw_fd() as u32,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        ensure!(
            observed == 0 && unsafe { info.si_pid() } == pid as i32,
            "original pidfd did not become terminal"
        );
        let code = info.si_code;
        let status = unsafe { info.si_status() };
        let reaped = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                fd.as_raw_fd() as u32,
                &mut info,
                libc::WEXITED | libc::WNOHANG,
            )
        };
        ensure!(
            reaped == 0
                && unsafe { info.si_pid() } == pid as i32
                && info.si_code == code
                && unsafe { info.si_status() } == status,
            "exact terminal pidfd reap changed"
        );
        let again = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                fd.as_raw_fd() as u32,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        ensure!(
            again == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
            "reaped original pidfd must return ECHILD"
        );
        Ok(
            json!({"pid":pid,"signal_sent":signal_sent,"waitid_code":code,
            "waitid_status":status,"reaped":true,"after_reap_echild":true}),
        )
    }

    struct LimitCustody {
        deadline: Instant,
        settled: bool,
    }

    impl LimitCustody {
        fn new(deadline: Instant) -> Result<Self> {
            let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
            ensure!(unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) } == 0);
            ensure!(
                action.sa_sigaction == libc::SIG_DFL && action.sa_flags & libc::SA_NOCLDWAIT == 0,
                "fresh custodian requires default SIGCHLD without automatic reap"
            );
            ensure!(limit_childless()?, "fresh custodian already owns a child");
            process_group::own_descendants()?;
            let mut subreaper = 0;
            ensure!(
                unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut subreaper, 0, 0, 0) } == 0
                    && subreaper == 1,
                "custodian subreaper was not installed"
            );
            Ok(Self {
                deadline,
                settled: false,
            })
        }

        fn settle(&mut self) -> Value {
            let mut retired = Vec::new();
            let mut errors = Vec::new();
            // The fixed fresh role can create only its isolated supervisor,
            // which can create only one observer. The second ancestry wave
            // covers adoption after retiring an intermediate in the first.
            for _ in 0..2 {
                let found = (|| -> Result<Vec<u32>> {
                    ensure!(Instant::now() < self.deadline, "custody deadline");
                    let mut children = Vec::new();
                    for entry in fs::read_dir("/proc")? {
                        ensure!(Instant::now() < self.deadline, "custody census deadline");
                        let entry = entry?;
                        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                            continue;
                        };
                        if pid == 0 {
                            continue;
                        }
                        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
                        let result = unsafe {
                            libc::waitid(
                                libc::P_PID,
                                pid,
                                &mut info,
                                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                            )
                        };
                        if result == -1 {
                            let error = std::io::Error::last_os_error();
                            ensure!(
                                error.raw_os_error() == Some(libc::ECHILD),
                                "candidate child authentication: {error}"
                            );
                            continue;
                        }
                        ensure!(
                            result == 0
                                && (unsafe { info.si_pid() } == 0
                                    || unsafe { info.si_pid() } == pid as i32),
                            "invalid child observation"
                        );
                        children.push(pid);
                        ensure!(
                            children.len() <= 2,
                            "fresh fixture child population exceeded"
                        );
                    }
                    Ok(children)
                })();
                match found {
                    Ok(children) => {
                        for pid in children {
                            match limit_retire_child(pid, self.deadline) {
                                Ok(receipt) => retired.push(receipt),
                                Err(error) => errors
                                    .push(format!("retire authenticated child {pid}: {error:#}")),
                            }
                        }
                    }
                    Err(error) => {
                        errors.push(format!("discover owned children: {error:#}"));
                        break;
                    }
                }
                match limit_childless() {
                    Ok(true) => {
                        self.settled = true;
                        break;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        errors.push(format!("final wait-set observation: {error:#}"));
                        break;
                    }
                }
            }
            if !self.settled {
                errors.push("actual child set is not empty".to_owned());
            }
            let within = Instant::now() < self.deadline;
            if !within {
                errors.push("original custody deadline exhausted".to_owned());
            }
            let complete = self.settled && within && errors.is_empty();
            json!({"initial_childless":true,"sigchld_default":true,"subreaper":true,
                "retired":retired,"final_echild":self.settled,"within_original_deadline":within,
                "cleanup_errors":errors,"cleanup_complete":complete})
        }
    }

    impl Drop for LimitCustody {
        fn drop(&mut self) {
            if !self.settled {
                let receipt = self.settle();
                eprintln!("limit custodian unwound: {receipt}, successful_receipt=false");
            }
        }
    }

    fn limit_observer_barrier() -> Result<()> {
        // Only the deliberate death fixture installs this file in the actual
        // observer's cwd. Normal observations always execute the old body.
        let path = Path::new("limit-custody-control.json");
        match fs::metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            other => {
                other?;
            }
        }
        let control: Value = serde_json::from_slice(&read_regular(path, 4096)?)?;
        ensure!(control["schema"] == 1);
        let fd = i32::try_from(control["fd"].as_u64().context("control fd")?)?;
        ensure!(fd > 2);
        let mut peer = unsafe { std::mem::zeroed::<libc::ucred>() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut peer as *mut libc::ucred).cast(),
                    &mut length,
                )
            } == 0
        );
        ensure!(
            length as usize == std::mem::size_of::<libc::ucred>()
                && control["custodian"] == peer.pid
                && peer.uid == unsafe { libc::geteuid() },
            "control socket peer mismatch"
        );
        let end = control["deadline_ns"]
            .as_u64()
            .context("control deadline")?;
        let observed = Instant::now();
        let remaining = end
            .checked_sub(limit_clock_ns()?)
            .context("observer deadline")?;
        // This byte deliberately conveys no PID or pidfd. The real observer
        // is now alive, before publishing its identity/limits, and waits.
        ensure!(unsafe { libc::write(fd, [0x52u8].as_ptr().cast(), 1) } == 1);
        limit_poll(fd, observed + Duration::from_nanos(remaining))?;
        anyhow::bail!("stalled observer released unexpectedly")
    }

    #[test]
    fn actual_package_step_applies_configured_caps() {
        let parent_before = actual_limits();
        let caps = [MAX_INPUT as libc::rlim_t, 512 * 1024 * 1024, 0];
        let expected: [[libc::rlim_t; 2]; 3] =
            std::array::from_fn(|index| parent_before[index].map(|value| value.min(caps[index])));
        // The focused normal-parent qualification requires this precondition.
        // Under the full suite's stricter soft limit the same test still checks
        // the exact minima, without raising any original-parent limit.
        let incoming_above_configured_caps = (0..2).all(|index| {
            parent_before[index]
                .iter()
                .all(|value| *value > caps[index])
        });
        let root = std::env::var_os("HERMIT_TEST_ACTION_RESULTS")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = root.join(format!(
            "package-limit-control-{}-ordinary",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        let result = step(
            &path,
            "observed",
            &[
                std::env::current_exe().unwrap().into_os_string(),
                "--exact".into(),
                "compiler_supervision_tests::actual_package_step_limit_observer".into(),
                "--nocapture".into(),
                "--test-threads=1".into(),
            ],
            Instant::now() + Duration::from_secs(30),
            None,
        );
        let parent_after = actual_limits();
        let receipt = fs::read(path.join("observed.json"))
            .map_err(anyhow::Error::from)
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(anyhow::Error::from));
        retain_limit_parent_record(&json!({
            "schema":1,"case":"ordinary","directory":path,
            "parent_pid":std::process::id(),
            "parent_limits_before":parent_before,"parent_limits_after":parent_after,
            "expected":expected,"incoming_above_configured_caps":incoming_above_configured_caps,
            "step_receipt":receipt.as_ref().ok(),
            "step_error":result.as_ref().err().map(|error| format!("{error:#}")),
            "receipt_error":receipt.as_ref().err().map(|error| format!("{error:#}"))
        }));
        // The actual step has completed its original-child cleanup before any
        // success assertion. Retain its error before a failed assertion exits.
        result.unwrap();
        assert_eq!(parent_after, parent_before, "parent limits changed");
        let receipt = receipt.unwrap();
        assert_eq!(receipt["returncode"], 0);
        assert_eq!(receipt["passed"], true);
        let owner = &receipt["supervision"];
        assert_eq!(owner["pid"], receipt["pid"]);
        assert_eq!(owner["raw_status"], 0);
        assert_eq!(owner["passed"], true);
        assert_eq!(owner["timed_out"], false);
        assert_eq!(owner["log_overflow"], false);
        assert_eq!(owner["primary_error"], Value::Null);
        assert_eq!(owner["terminal_bounds_error"], Value::Null);
        assert_eq!(owner["terminal_observed_without_reap"], true);
        assert_eq!(owner["natural_terminal_group"], true);
        assert_eq!(owner["owned_group_kill_before_reap"], true);
        assert_eq!(owner["unreaped_child_retained_until_receipt"], false);
        assert_eq!(owner["cleanup_complete"], true);
        assert_eq!(owner["cleanup_errors"], json!([]));
        assert_eq!(owner["final_group_absent"], true);
        let output = fs::read_to_string(path.join("observed.stdout")).unwrap();
        let records = output
            .lines()
            .filter_map(|line| line.strip_prefix("PACKAGE-LIMIT-OBSERVATION "))
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1, "one genuine child's observation");
        let observed: Value = serde_json::from_str(records[0]).unwrap();
        assert_eq!(observed["pid"], receipt["pid"]);
        assert_eq!(observed["parent"], std::process::id());
        assert_eq!(observed["limits"], json!(expected));
    }

    fn inherited_limit_outer(case: &str, selector: &str, output_cap: libc::rlim_t) {
        let parent_before = actual_limits();
        assert!(std::env::var_os("HERMIT_PACKAGE_LIMIT_ROLE").is_none());
        assert!(std::env::var_os("HERMIT_PACKAGE_LIMIT_WORK").is_none());
        assert!(std::env::var_os("HERMIT_PACKAGE_LIMIT_DEADLINE").is_none());
        let root = std::env::var_os("HERMIT_TEST_ACTION_RESULTS")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = root.join(format!(
            "package-limit-control-{}-{case}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("step")).unwrap();
        let stdout = path.join("custodian.stdout");
        let stderr = path.join("custodian.stderr");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", selector, "--nocapture", "--test-threads=1"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .env("HERMIT_PACKAGE_LIMIT_ROLE", format!("custodian-{case}"))
            .env("HERMIT_PACKAGE_LIMIT_WORK", &path)
            .stdin(Stdio::null())
            .stdout(
                File::options()
                    .write(true)
                    .create_new(true)
                    .open(&stdout)
                    .unwrap(),
            )
            .stderr(
                File::options()
                    .write(true)
                    .create_new(true)
                    .open(&stderr)
                    .unwrap(),
            );
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut action = std::mem::zeroed::<libc::sigaction>();
                action.sa_sigaction = libc::SIG_DFL;
                if libc::sigemptyset(&mut action.sa_mask) != 0
                    || libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                let mut limits = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(libc::RLIMIT_FSIZE, &mut limits) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                limits.rlim_cur = limits.rlim_cur.min(output_cap);
                if libc::setrlimit(libc::RLIMIT_FSIZE, &limits) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        process_group::own_descendants().unwrap();
        // One origin for all nested budgets. Inner stages reserve cleanup time;
        // they never install another fresh30-second deadline.
        let end = limit_clock_ns()
            .unwrap()
            .checked_add(30_000_000_000)
            .unwrap();
        command.env("HERMIT_PACKAGE_LIMIT_DEADLINE", end.to_string());
        let started = Instant::now();
        let owner = OwnedChild {
            child: command.spawn().unwrap(),
            cleanup_attempted: false,
            reaped: false,
        };
        let original = owner.child.id();
        let receipt = supervise(
            owner,
            started,
            &stdout,
            &stderr,
            Limits {
                wall: Duration::from_secs(30),
                logs: 2 * MAX_INPUT as u64,
                cleanup: Duration::from_secs(2),
            },
        );
        emit_limit_custodian_receipt(&path.join("custodian.json"), &receipt).unwrap();
        // The outer original child is retired before any result comparison.
        assert_eq!(actual_limits(), parent_before, "parent limits changed");
        assert_eq!(receipt["pid"], original);
        assert_eq!(receipt["raw_status"], if case == "death" { 101 } else { 0 });
        assert_eq!(receipt["passed"], case != "death");
        assert_eq!(receipt["signal"], Value::Null);
        assert_eq!(receipt["timed_out"], false);
        assert_eq!(receipt["log_overflow"], false);
        assert_eq!(receipt["primary_error"], Value::Null);
        assert_eq!(receipt["terminal_bounds_error"], Value::Null);
        assert_eq!(receipt["terminal_observed_without_reap"], true);
        assert_eq!(receipt["natural_terminal_group"], true);
        assert_eq!(receipt["owned_group_kill_before_reap"], true);
        assert_eq!(receipt["unreaped_child_retained_until_receipt"], false);
        assert_eq!(receipt["cleanup_complete"], true);
        assert_eq!(receipt["cleanup_errors"], json!([]));
        assert_eq!(receipt["final_group_absent"], true);
        let output = fs::read_to_string(&stdout).unwrap();
        let rows = output
            .lines()
            .filter_map(|line| line.strip_prefix("PACKAGE-LIMIT-CUSTODY "))
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1, "one fresh custodian's actual cleanup record");
        let row: Value = serde_json::from_str(rows[0]).unwrap();
        assert_eq!(row["custodian"], original);
        assert_eq!(row["case"], case);
        assert_eq!(row["parent_limits_before"], row["parent_limits_after"]);
        let custody = &row["custody"];
        assert_eq!(custody["initial_childless"], true);
        assert_eq!(custody["sigchld_default"], true);
        assert_eq!(custody["subreaper"], true);
        assert_eq!(custody["final_echild"], true);
        assert_eq!(custody["within_original_deadline"], true);
        assert_eq!(custody["cleanup_complete"], true);
        assert_eq!(custody["cleanup_errors"], json!([]));
        if case == "death" {
            // A failed real stage remains failed. This control succeeds only
            // by proving that precise failure followed authentic retirement.
            assert_eq!(row["injection"]["ready_byte"], 0x52);
            assert_eq!(row["injection"]["signaled_pid"], row["supervisor"]["pid"]);
            assert_eq!(row["injection"]["observer_identity_published"], false);
            assert_eq!(row["supervisor"]["passed"], false);
            assert_eq!(row["supervisor"]["raw_status"], Value::Null);
            assert_eq!(row["supervisor"]["signal"], libc::SIGKILL);
            assert_eq!(row["supervisor"]["timed_out"], false);
            assert_eq!(row["supervisor"]["cleanup_complete"], true);
            assert_eq!(row["supervisor"]["cleanup_errors"], json!([]));
            let retired = custody["retired"].as_array().unwrap();
            assert_eq!(retired.len(), 1, "the actual adopted observer");
            assert_ne!(retired[0]["pid"], row["supervisor"]["pid"]);
            assert_eq!(retired[0]["signal_sent"], true);
            assert_eq!(retired[0]["waitid_code"], libc::CLD_KILLED);
            assert_eq!(retired[0]["waitid_status"], libc::SIGKILL);
            assert_eq!(retired[0]["reaped"], true);
            assert_eq!(retired[0]["after_reap_echild"], true);
            let observer_output = fs::read_to_string(path.join("step/observed.stdout")).unwrap();
            assert!(!observer_output.contains("PACKAGE-LIMIT-OBSERVATION "));
            assert!(
                !path.join("step/observed.json").exists(),
                "killed supervisor cannot manufacture a completed step receipt"
            );
        } else {
            assert_eq!(row["injection"], Value::Null);
            assert_eq!(
                custody["retired"],
                json!([]),
                "normal completion cannot depend on orphan cleanup"
            );
        }
    }

    fn inherited_limit_case(
        case: &str,
        selector: &str,
        file: [libc::rlim_t; 2],
        memory: [libc::rlim_t; 2],
    ) {
        let role = std::env::var_os("HERMIT_PACKAGE_LIMIT_ROLE");
        if role.is_none() {
            inherited_limit_outer(case, selector, file[0]);
            return;
        }
        if role == Some(OsString::from(case)) {
            let work = PathBuf::from(std::env::var_os("HERMIT_PACKAGE_LIMIT_WORK").unwrap());
            emit(
                &work.join("inherited.json"),
                &json!({"pid":std::process::id(),"limits":actual_limits()}),
            )
            .unwrap();
            // No comparison with expected caps here: for the soft regression
            // both old and fixed executions must be able to exit0. The original
            // parent checks the observer's values only after genuine cleanup.
            step(
                &work,
                "observed",
                &[
                    std::env::current_exe().unwrap().into_os_string(),
                    "--exact".into(),
                    "compiler_supervision_tests::actual_package_step_limit_observer".into(),
                    "--nocapture".into(),
                    "--test-threads=1".into(),
                ],
                limit_deadline(10).unwrap(),
                None,
            )
            .unwrap();
            return;
        }

        assert_eq!(role, Some(OsString::from(format!("custodian-{case}"))));
        let mut custody = LimitCustody::new(limit_deadline(2).unwrap()).unwrap();
        let supervisor_deadline = limit_deadline(6).unwrap();
        let observer_deadline = limit_deadline(10).unwrap();
        let parent_before = actual_limits();
        for (inherited, wanted) in parent_before.iter().zip([file, memory, [0, 0]]) {
            assert!(inherited[0] >= wanted[0] && inherited[1] >= wanted[1]);
        }
        let path = PathBuf::from(std::env::var_os("HERMIT_PACKAGE_LIMIT_WORK").unwrap());
        let work = path.join("step");
        let stdout = path.join("stdout");
        let stderr = path.join("stderr");
        use std::os::fd::AsRawFd;
        let control = if case == "death" {
            let pair = std::os::unix::net::UnixStream::pair().unwrap();
            let end = std::env::var("HERMIT_PACKAGE_LIMIT_DEADLINE")
                .unwrap()
                .parse::<u64>()
                .unwrap();
            emit(
                &work.join("limit-custody-control.json"),
                &json!({
                    "schema":1,"fd":pair.1.as_raw_fd(),"custodian":std::process::id(),
                    "deadline_ns":end.checked_sub(10_000_000_000).unwrap()
                }),
            )
            .unwrap();
            Some(pair)
        } else {
            None
        };
        let control_fd = control.as_ref().map(|pair| pair.1.as_raw_fd());
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", selector, "--nocapture", "--test-threads=1"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .env("HERMIT_PACKAGE_LIMIT_ROLE", case)
            .env("HERMIT_PACKAGE_LIMIT_WORK", &work)
            .env(
                "HERMIT_PACKAGE_LIMIT_DEADLINE",
                std::env::var_os("HERMIT_PACKAGE_LIMIT_DEADLINE").unwrap(),
            )
            .current_dir(&work)
            .stdin(Stdio::null())
            .stdout(
                File::options()
                    .write(true)
                    .create_new(true)
                    .open(&stdout)
                    .unwrap(),
            )
            .stderr(
                File::options()
                    .write(true)
                    .create_new(true)
                    .open(&stderr)
                    .unwrap(),
            );
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(fd) = control_fd {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                for (resource, [soft, hard]) in [
                    (libc::RLIMIT_FSIZE, file),
                    (libc::RLIMIT_AS, memory),
                    (libc::RLIMIT_CORE, [0, 0]),
                ] {
                    let mut inherited = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    if libc::getrlimit(resource, &mut inherited) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // The fixture may only lower the original child's limits.
                    if soft > inherited.rlim_cur || hard > inherited.rlim_max {
                        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                    }
                    let value = libc::rlimit {
                        rlim_cur: soft,
                        rlim_max: hard,
                    };
                    if libc::setrlimit(resource, &value) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        process_group::own_descendants().unwrap();
        let started = Instant::now();
        let owner = OwnedChild {
            child: command.spawn().unwrap(),
            cleanup_attempted: false,
            reaped: false,
        };
        let original = owner.child.id();
        let mut control = control.map(|(reader, writer)| {
            drop(writer);
            reader
        });
        let injection = (|| -> Result<Value> {
            let Some(socket) = control.as_mut() else {
                return Ok(Value::Null);
            };
            ensure!(
                !process_group::exited_without_reap(&owner.child)?,
                "original supervisor exited before injection setup"
            );
            let original_fd = limit_pidfd(original)?;
            limit_poll(socket.as_raw_fd(), observer_deadline)?;
            let mut byte = [0u8];
            std::io::Read::read_exact(socket, &mut byte)?;
            ensure!(byte == [0x52], "actual observer readiness byte");
            ensure!(
                !process_group::exited_without_reap(&owner.child)?,
                "original supervisor exited before the causal kill"
            );
            limit_kill(&original_fd)?;
            Ok(json!({"ready_byte":byte[0],"signaled_pid":original,
                "observer_identity_published":false}))
        })();
        let receipt = supervise(
            owner,
            started,
            &stdout,
            &stderr,
            Limits {
                wall: supervisor_deadline.saturating_duration_since(started),
                logs: 2 * MAX_INPUT as u64,
                cleanup: Duration::from_secs(2),
            },
        );
        // Keep the socket peer alive while retiring the observer, so EOF cannot
        // masquerade as the required killed-child result.
        let custody_receipt = custody.settle();
        emit(&path.join("result.json"), &receipt).unwrap();
        {
            let mut output = std::io::stdout().lock();
            output.write_all(b"\nPACKAGE-LIMIT-CUSTODY ").unwrap();
            serde_json::to_writer(
                &mut output,
                &json!({
                    "custodian":std::process::id(),"case":case,
                    "parent_limits_before":parent_before,"parent_limits_after":actual_limits(),
                    "supervisor":receipt,"custody":custody_receipt,
                    "injection":injection.as_ref().ok(),
                    "injection_error":injection.as_ref().err().map(|error| format!("{error:#}"))
                }),
            )
            .unwrap();
            output.write_all(b"\n").unwrap();
            output.flush().unwrap();
        }
        injection.unwrap();
        assert_eq!(custody_receipt["cleanup_complete"], true);
        assert_eq!(custody_receipt["cleanup_errors"], json!([]));
        assert_eq!(custody_receipt["final_echild"], true);
        // Every assertion below follows the original parent's actual cleanup.
        assert_eq!(actual_limits(), parent_before, "parent limits changed");
        assert_eq!(receipt["pid"], original);
        assert_eq!(receipt["raw_status"], 0);
        assert_eq!(receipt["passed"], true);
        assert_eq!(receipt["timed_out"], false);
        assert_eq!(receipt["log_overflow"], false);
        assert_eq!(receipt["primary_error"], Value::Null);
        assert_eq!(receipt["terminal_bounds_error"], Value::Null);
        assert_eq!(receipt["terminal_observed_without_reap"], true);
        assert_eq!(receipt["natural_terminal_group"], true);
        assert_eq!(receipt["owned_group_kill_before_reap"], true);
        assert_eq!(receipt["unreaped_child_retained_until_receipt"], false);
        assert_eq!(receipt["cleanup_complete"], true);
        assert_eq!(receipt["cleanup_errors"], json!([]));
        assert_eq!(receipt["final_group_absent"], true);
        let inherited: Value =
            serde_json::from_slice(&fs::read(work.join("inherited.json")).unwrap()).unwrap();
        assert_eq!(inherited["pid"], original);
        assert_eq!(inherited["limits"], json!([file, memory, [0, 0]]));
        let step_receipt: Value =
            serde_json::from_slice(&fs::read(work.join("observed.json")).unwrap()).unwrap();
        assert_eq!(step_receipt["returncode"], 0);
        assert_eq!(step_receipt["passed"], true);
        let step_owner = &step_receipt["supervision"];
        assert_eq!(step_owner["terminal_observed_without_reap"], true);
        assert_eq!(step_owner["natural_terminal_group"], true);
        assert_eq!(step_owner["owned_group_kill_before_reap"], true);
        assert_eq!(step_owner["unreaped_child_retained_until_receipt"], false);
        assert_eq!(step_owner["cleanup_complete"], true);
        assert_eq!(step_owner["cleanup_errors"], json!([]));
        assert_eq!(step_owner["final_group_absent"], true);
        let output = fs::read_to_string(work.join("observed.stdout")).unwrap();
        let records = output
            .lines()
            .filter_map(|line| line.strip_prefix("PACKAGE-LIMIT-OBSERVATION "))
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1, "one genuine child's observation");
        let observed: Value = serde_json::from_str(records[0]).unwrap();
        assert_eq!(observed["pid"], step_receipt["pid"]);
        assert_eq!(observed["parent"], original);
        assert_eq!(observed["limits"], json!([file, memory, [0, 0]]));
    }

    #[test]
    fn actual_package_step_respects_lower_hard_limits() {
        inherited_limit_case(
            "hard",
            "compiler_supervision_tests::actual_package_step_respects_lower_hard_limits",
            [32768, 32768],
            [256 * 1024 * 1024, 256 * 1024 * 1024],
        );
    }

    #[test]
    fn actual_package_step_preserves_lower_soft_limits() {
        inherited_limit_case(
            "soft",
            "compiler_supervision_tests::actual_package_step_preserves_lower_soft_limits",
            [8192, MAX_INPUT as libc::rlim_t],
            [256 * 1024 * 1024, 512 * 1024 * 1024],
        );
    }

    #[test]
    fn actual_package_step_nested_supervisor_death_retires_observer() {
        inherited_limit_case(
            "death",
            "compiler_supervision_tests::actual_package_step_nested_supervisor_death_retires_observer",
            [8192, MAX_INPUT as libc::rlim_t],
            [256 * 1024 * 1024, 512 * 1024 * 1024],
        );
    }
}

#[cfg(test)]
mod scratch_environment_tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn evidence() -> PathBuf {
        let root = std::env::var_os("HERMIT_TEST_ACTION_RESULTS")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = root.join(format!(
            "package-scratch-control-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path.canonicalize().unwrap()
    }

    fn parent_environment() -> BTreeMap<OsString, OsString> {
        std::env::vars_os().collect()
    }

    fn natural_cleanup(receipt: &Value) {
        let owner = &receipt["supervision"];
        assert_eq!(owner["pid"], receipt["pid"]);
        assert_eq!(owner["raw_status"], receipt["returncode"]);
        assert_eq!(receipt["signal"], Value::Null);
        assert_eq!(owner["signal"], Value::Null);
        assert_eq!(owner["timed_out"], false);
        assert_eq!(owner["log_overflow"], false);
        assert_eq!(owner["primary_error"], Value::Null);
        assert_eq!(owner["terminal_bounds_error"], Value::Null);
        assert_eq!(owner["terminal_observed_without_reap"], true);
        assert_eq!(owner["natural_terminal_group"], true);
        assert_eq!(owner["group_members_before_kill"], json!([receipt["pid"]]));
        assert_eq!(owner["naturally_reaped_descendants"], json!([]));
        assert_eq!(owner["cleanup_reaped_descendants"], json!([]));
        assert_eq!(owner["owned_group_kill_before_reap"], true);
        assert_eq!(owner["cleanup_attempted"], true);
        assert_eq!(owner["cleanup_complete"], true);
        assert_eq!(owner["cleanup_within_bound"], true);
        assert_eq!(owner["unreaped_child_retained_until_receipt"], false);
        assert_eq!(owner["cleanup_errors"], json!([]));
        assert_eq!(owner["final_group_absent"], true);
        assert_eq!(receipt["group_absent"], true);
        // The original supervise() already consumed Child::wait(). This
        // nonconsuming observation must now report that no such child exists.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe {
                libc::waitid(
                    libc::P_PID,
                    receipt["pid"].as_u64().unwrap() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn actual_step_owns_tmpdir() {
        let work = evidence();
        let before = parent_environment();
        // Refuse BEFORE tempfile can fall back to /tmp. The expected directory
        // is a literal argv value, independent of the child's environment.
        // -I/-B prevent user startup hooks and Python bytecode cache writes.
        let script = r#"
import json, os, sys
expected = sys.argv[1]
actual = os.environ.get('TMPDIR')
row = {'pid': os.getpid(), 'expected': expected, 'actual': actual,
       'path': None, 'content': None}
if actual != expected:
    print(json.dumps(row), flush=True)
    sys.exit(73)
import tempfile
fd, path = tempfile.mkstemp(prefix='provider-step-scratch-')
with os.fdopen(fd, 'wb') as stream:
    stream.write(b'provider scratch\n')
with open(path, 'rb') as stream:
    row.update(path=os.path.realpath(path), content=stream.read().decode('ascii'))
print(json.dumps(row), flush=True)
"#;
        let result = step(
            &work,
            "scratch",
            &[
                "/usr/bin/python3".into(),
                "-I".into(),
                "-B".into(),
                "-c".into(),
                script.into(),
                work.as_os_str().to_owned(),
            ],
            Instant::now() + Duration::from_secs(30),
            None,
        );
        let receipt: Value =
            serde_json::from_slice(&fs::read(work.join("scratch.json")).unwrap()).unwrap();
        let observed: Value =
            serde_json::from_slice(&fs::read(work.join("scratch.stdout")).unwrap()).unwrap();
        let unchanged = before == parent_environment();
        let record = json!({"work":work,"observation":observed,"receipt":receipt,
            "parent_tmpdir":before.get(&OsString::from("TMPDIR")).map(PathBuf::from),
            "parent_environment_unchanged":unchanged,
            "step_error":result.as_ref().err().map(|error| format!("{error:#}"))});
        emit(&work.join("parent.json"), &record).unwrap();
        println!("PACKAGE-SCRATCH-OBSERVATION {record}");
        // These checks run after the unchanged authentic child cleanup, even
        // on the old producer's exit73. That failure is never labelled a pass.
        natural_cleanup(&receipt);
        assert!(unchanged, "parent environment changed");
        assert_eq!(observed["pid"], receipt["pid"]);
        let matched = observed["actual"] == observed["expected"];
        assert_eq!(receipt["returncode"], if matched { 0 } else { 73 });
        assert_eq!(receipt["passed"], matched);
        assert_eq!(result.is_ok(), matched);
        assert_eq!(observed["expected"], json!(work));
        assert_eq!(
            observed["actual"],
            json!(work),
            "scratch ownership mismatch"
        );
        result.unwrap();
        assert_eq!(receipt["returncode"], 0);
        let path = PathBuf::from(observed["path"].as_str().unwrap());
        assert_eq!(path.parent(), Some(work.as_path()));
        assert_eq!(path.canonicalize().unwrap(), path);
        assert!(fs::symlink_metadata(&path).unwrap().is_file());
        assert_eq!(observed["content"], "provider scratch\n");
        assert_eq!(fs::read(&path).unwrap(), b"provider scratch\n");
        assert!(fs::read(work.join("scratch.stderr")).unwrap().is_empty());
    }

    #[test]
    fn actual_step_does_not_forward_ambient_tmpdir() {
        let work = evidence();
        let ambient = work.join("adversarial-ambient");
        fs::create_dir(&ambient).unwrap();
        let before = parent_environment();
        // Change only a fresh, genuinely supervised child's environment. The
        // parallel libtest parent never calls set_var or remove_var. This uses
        // the same step/OwnedChild lifecycle, not a separate launcher protocol.
        let result = step(
            &work,
            "ambient",
            &[
                "/usr/bin/env".into(),
                format!("TMPDIR={}", ambient.display()).into(),
                format!("HERMIT_TEST_ACTION_RESULTS={}", work.display()).into(),
                std::env::current_exe().unwrap().into_os_string(),
                "--exact".into(),
                "scratch_environment_tests::actual_step_owns_tmpdir".into(),
                "--nocapture".into(),
                "--test-threads=1".into(),
            ],
            Instant::now() + Duration::from_secs(30),
            None,
        );
        let receipt: Value =
            serde_json::from_slice(&fs::read(work.join("ambient.json")).unwrap()).unwrap();
        let unchanged = before == parent_environment();
        let empty = fs::read_dir(&ambient).unwrap().next().is_none();
        emit(
            &work.join("parent.json"),
            &json!({"receipt":receipt,
            "ambient":ambient,"ambient_empty":empty,"parent_environment_unchanged":unchanged,
            "step_error":result.as_ref().err().map(|error| format!("{error:#}"))}),
        )
        .unwrap();
        natural_cleanup(&receipt);
        assert!(unchanged, "parent environment changed");
        assert!(
            empty,
            "scratch escaped into the adversarial ambient directory"
        );
        result.unwrap();
        assert_eq!(receipt["returncode"], 0);
        assert_eq!(receipt["passed"], true);
        let nested = fs::read_dir(&work)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.join("scratch.json").is_file())
            .collect::<Vec<_>>();
        assert_eq!(nested.len(), 1);
        let child_record: Value =
            serde_json::from_slice(&fs::read(nested[0].join("parent.json")).unwrap()).unwrap();
        assert_eq!(child_record["parent_tmpdir"], json!(ambient));
        assert_eq!(child_record["parent_environment_unchanged"], true);
        assert_eq!(child_record["observation"]["actual"], json!(nested[0]));
        assert_ne!(child_record["observation"]["actual"], json!(ambient));
    }
}
