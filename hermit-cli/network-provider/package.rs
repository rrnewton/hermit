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
//! anyhow = "=1.0.100"
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
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use package_support::{
    Contract, MAX_ARTIFACT, MAX_INPUT, digest, prove_compression, read_regular, validate_elf,
};
use serde_json::{Value, json};

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

fn group_absent(pid: u32) -> Result<bool> {
    // This group was created by setsid in our compiler child. No helper or
    // privileged process is ever launched by this action.
    let result = unsafe { libc::kill(-(pid as i32), 0) };
    if result == 0 {
        return Ok(false);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(true)
    } else {
        Err(error.into())
    }
}

fn kill_group(pid: u32) -> Result<()> {
    let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

/// Cleanup for compiler errors/panics only; this does not own effectful sockets.
/// Drop is not used as a successful cleanup receipt.
struct CompilerChild {
    child: Child,
    active: bool,
}

impl Drop for CompilerChild {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _ = kill_group(self.child.id());
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if self.child.try_wait().ok().flatten().is_some()
                && group_absent(self.child.id()).unwrap_or(false)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

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
        .open(output_path)?;
    let error = File::options()
        .write(true)
        .create_new(true)
        .open(work.join(format!("{name}.stderr")))?;
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
    let child = command.spawn();
    if let Err(error) = &child {
        emit(
            &work.join(format!("{name}.json")),
            &json!({"argv":argv_json,"spawn_error":error.to_string(),"returncode":null,"passed":false}),
        )?;
    }
    let mut child = CompilerChild {
        child: child.with_context(|| format!("start {name}"))?,
        active: true,
    };
    let pid = child.child.id();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.child.try_wait()? {
            break status;
        }
        if started.elapsed() >= remaining {
            timed_out = true;
            kill_group(pid)?;
            let until = Instant::now() + Duration::from_secs(2);
            let killed = loop {
                if let Some(status) = child.child.try_wait()? {
                    break status;
                }
                ensure!(
                    Instant::now() < until,
                    "compiler child did not reap within two-second kill bound"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            break killed;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let absent = group_absent(pid)?;
    let mut remaining_group_killed = None;
    if !absent {
        kill_group(pid)?;
        let until = Instant::now() + Duration::from_secs(2);
        while !group_absent(pid)? && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        remaining_group_killed = Some(group_absent(pid)?);
    }
    if absent || remaining_group_killed == Some(true) {
        child.active = false;
    }
    let passed = status.success() && !timed_out && absent;
    emit(
        &work.join(format!("{name}.json")),
        &json!({"argv":argv_json,"pid":pid,"returncode":status.code(),"signal":status.signal(),"timed_out":timed_out,"group_absent":absent,"remaining_group_killed":remaining_group_killed,"elapsed_seconds":started.elapsed().as_secs_f64(),"passed":passed}),
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
    let mut names = contract.source_files.clone();
    names.extend([
        contract_name,
        "package.rs".to_owned(),
        "package_support.rs".to_owned(),
    ]);
    let source_digests = source_hashes(&args.source, &names)?;
    let btf = read_regular(Path::new("/sys/kernel/btf/vmlinux"), MAX_INPUT)?;
    let checked_btf = contract.require_btf(&btf)?;
    let accepted = args.component == "accepted";
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
    if !accepted {
        artifacts.insert("helper_sha256", "hermit-unix-keeper");
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
                    && manifest["btf_sha256"] == contract.btf_sha256
                    && manifest["sources"] == serde_json::to_value(&source_digests)?
                    && manifest["maps"] == contract.maps
                    && manifest["programs"] == contract.programs
                    && manifest["links"] == contract.links,
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
    let flags = vec![
        args.clang.as_os_str().to_owned(),
        os("-O2"),
        os("-g"),
        os("-Wall"),
        os("-Wextra"),
        os("-Werror"),
    ];
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
        src.join(if accepted {
            "provider.bpf.c"
        } else {
            "unix-guard.bpf.c"
        })
        .into_os_string(),
        os("-o"),
        work.join("uncompressed.bpf.o").into_os_string(),
    ]);
    step(&work, "bpf", &bpf, deadline, None)?;
    let mut shared = flags.clone();
    shared.extend([os("-shared"), os("-fPIC")]);
    if accepted {
        shared.extend([
            src.join("driver.c").into_os_string(),
            src.join("adapter-abi.c").into_os_string(),
            args.libbpf.as_os_str().to_owned(),
        ]);
    } else {
        shared.extend([
            src.join("keeper-channel.c").into_os_string(),
            src.join("keeper-monitor.c").into_os_string(),
        ]);
    }
    shared.extend([os("-o"), out.join(library_name).into_os_string()]);
    step(&work, "shared", &shared, deadline, None)?;
    if !accepted {
        let mut helper = flags;
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
    if !accepted {
        let helper = read_regular(&out.join("hermit-unix-keeper"), MAX_ARTIFACT)?;
        validate_elf(&helper, 62, 3)?;
        manifest["helper"] = json!("hermit-unix-keeper");
        manifest["helper_sha256"] = json!(digest(&helper));
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
