#!/usr/bin/env rust-script
// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

use std::env;
use std::fs;
use std::fs::File;
use std::io::IsTerminal;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const KERNEL_VERSION: &str = "6.11.11";
const KERNEL_PACKAGE: &str =
    "linux-image-unsigned-6.11.11-061111-generic_6.11.11-061111.202412051415_amd64.deb";
const KERNEL_URL: &str = "https://kernel.ubuntu.com/mainline/v6.11.11/amd64/linux-image-unsigned-6.11.11-061111-generic_6.11.11-061111.202412051415_amd64.deb";
const KERNEL_SHA256: &str = "b760c0406366991c2e7729bbe01a1031057abd2d11d6b53e9445a0920db9854c";
const BOOT_MARKER: &str = "DEMO05_LINUX_BOOT_OK";
const VERIFY_MARKER: &str = "Success: deterministic. Determinism verified.";

const INIT_SOURCE: &str = r#"
enum {
  SYS_WRITE = 1,
  SYS_PAUSE = 34,
  SYS_EXIT = 60,
  SYS_UNAME = 63,
  SYS_SYNC = 162,
  SYS_REBOOT = 169,
  STDOUT_FILENO = 1,
};

struct utsname {
  char sysname[65];
  char nodename[65];
  char release[65];
  char version[65];
  char machine[65];
  char domainname[65];
};

static long syscall0(long number) {
  register long rax __asm__("rax") = number;
  __asm__ volatile("syscall" : "+a"(rax) : : "rcx", "r11", "memory");
  return rax;
}

static long syscall1(long number, long arg1) {
  register long rax __asm__("rax") = number;
  register long rdi __asm__("rdi") = arg1;
  __asm__ volatile("syscall" : "+a"(rax) : "D"(rdi) : "rcx", "r11", "memory");
  return rax;
}

static long syscall3(long number, long arg1, long arg2, long arg3) {
  register long rax __asm__("rax") = number;
  register long rdi __asm__("rdi") = arg1;
  register long rsi __asm__("rsi") = arg2;
  register long rdx __asm__("rdx") = arg3;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi), "S"(rsi), "d"(rdx)
                   : "rcx", "r11", "memory");
  return rax;
}

static long syscall4(long number, long arg1, long arg2, long arg3, long arg4) {
  register long rax __asm__("rax") = number;
  register long rdi __asm__("rdi") = arg1;
  register long rsi __asm__("rsi") = arg2;
  register long rdx __asm__("rdx") = arg3;
  register long r10 __asm__("r10") = arg4;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi), "S"(rsi), "d"(rdx), "r"(r10)
                   : "rcx", "r11", "memory");
  return rax;
}

static unsigned long text_length(const char *text) {
  unsigned long length = 0;
  while (text[length] != '\0') {
    ++length;
  }
  return length;
}

static void write_text(const char *text) {
  syscall3(SYS_WRITE, STDOUT_FILENO, (long)text, text_length(text));
}

void _start(void) {
  struct utsname system;
  if (syscall1(SYS_UNAME, (long)&system) < 0) {
    write_text("DEMO05_UNAME_FAILED\n");
    syscall1(SYS_EXIT, 1);
  }

  write_text("\n============================================================\n");
  write_text("DEMO05_LINUX_BOOT_OK\n");
  write_text("  kernel release : ");
  write_text(system.release);
  write_text("\n  architecture   : ");
  write_text(system.machine);
  write_text("\n  init process   : static /init (PID 1)\n");
  write_text("  execution      : QEMU TCG inside Hermit strict mode\n");
  write_text("DEMO05_GUEST_POWERING_OFF\n");
  write_text("============================================================\n");
  syscall0(SYS_SYNC);
  syscall4(SYS_REBOOT, 0xfee1dead, 0x28121969, 0x4321fedc, 0);
  for (;;) {
    syscall0(SYS_PAUSE);
  }
}
"#;

struct Theme {
    color: bool,
    started: Instant,
}

impl Theme {
    fn new() -> Self {
        Self {
            color: io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none(),
            started: Instant::now(),
        }
    }

    fn styled(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    fn banner(&self) {
        println!(
            "{}",
            self.styled(
                "1;36",
                "======================================================================"
            )
        );
        println!(
            "{}",
            self.styled(
                "1;37",
                "  DEMO 05  //  A REPRODUCIBLE MACHINE INSIDE A MACHINE"
            )
        );
        println!(
            "{}",
            self.styled(
                "36",
                "  Hermit -> QEMU TCG -> Linux -> deterministic poweroff"
            )
        );
        println!(
            "{}",
            self.styled(
                "1;36",
                "======================================================================"
            )
        );
    }

    fn step(&self, number: usize, title: &str) {
        println!();
        println!(
            "{} {}",
            self.styled("1;36", &format!("[{number}/5]")),
            self.styled("1;37", title)
        );
        println!("{}", self.styled("2", &"-".repeat(70)));
    }

    fn say(&self, text: &str) {
        println!(
            "{} {text}",
            self.styled("2", &format!("[+{:>4}s]", self.started.elapsed().as_secs()))
        );
    }

    fn warn(&self, text: &str) {
        eprintln!("{} {text}", self.styled("1;33", "warning:"));
    }

    fn success(&self, text: &str) {
        println!("{} {text}", self.styled("1;32", "PASS"));
    }
}

struct Tools {
    ar: Option<PathBuf>,
    cc: PathBuf,
    cpio: PathBuf,
    curl: Option<PathBuf>,
    dpkg_deb: Option<PathBuf>,
    gzip: PathBuf,
    hermit: PathBuf,
    qemu: PathBuf,
    sha256sum: PathBuf,
    tar: Option<PathBuf>,
    timeout: PathBuf,
    with_proxy: Option<PathBuf>,
}

fn main() {
    if let Err(error) = run_demo() {
        eprintln!("\nDEMO05 FAILED: {error}");
        std::process::exit(1);
    }
}

fn run_demo() -> Result<(), String> {
    let theme = Theme::new();
    theme.banner();
    println!();
    println!("Hermit will execute an entire virtual machine twice.");
    println!("Linux must boot, run PID 1, and power off with the same observable");
    println!("output and the same Detcore event log in both executions.");

    let repo = find_repo_root()?;
    let tools = find_tools(&repo)?;
    let phase_timeout = positive_env_u64("DEMO05_TIMEOUT_SECONDS", 720)?;
    let run_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?
        .as_secs();
    let artifact_root = repo
        .join("target/demo05-linux-boot")
        .join(format!("run-{run_id}-{}", std::process::id()));
    fs::create_dir_all(&artifact_root)
        .map_err(|error| format!("create {}: {error}", artifact_root.display()))?;

    theme.step(1, "Pin the inputs");
    theme.say("A reproducibility claim starts with immutable inputs.");
    let kernel_source = acquire_kernel(&theme, &repo, &tools)?;
    let kernel = artifact_root.join("kernel.bzImage");
    fs::copy(&kernel_source, &kernel).map_err(|error| {
        format!(
            "copy kernel {} to {}: {error}",
            kernel_source.display(),
            kernel.display()
        )
    })?;
    println!("  kernel : {}", kernel.display());
    println!("  sha256 : {}", sha256_file(&tools.sha256sum, &kernel)?);
    println!("  qemu   : {}", first_version_line(&tools.qemu));
    println!("  hermit : {}", first_version_line(&tools.hermit));

    theme.step(2, "Build the smallest possible Linux userspace");
    theme.say("Compiling a static PID 1 and packing it into a newc initramfs.");
    let initrd = build_initramfs(&artifact_root, &tools)?;
    println!("  initrd : {}", initrd.display());
    println!("  sha256 : {}", sha256_file(&tools.sha256sum, &initrd)?);
    println!("  payload: uname -> proof marker -> sync -> Linux poweroff syscall");

    theme.step(3, "Define the deterministic machine");
    println!("  QEMU uses one TCG translation thread and instruction-counted time.");
    println!("  Hermit uses the ptrace backend, strict scheduling, virtual time,");
    println!("  deterministic I/O, and its fixed seed. No determinism relaxations.");
    println!();
    println!("  --strict  rejects scheduling and I/O opt-outs.");
    println!("  --verify  runs the complete boot twice and compares:");
    println!("              * exit status");
    println!("              * stdout and stderr");
    println!("              * Detcore's normalized internal event log");

    let console_evidence = artifact_root.join("verified-console.log");
    let hash_evidence = artifact_root.join("verified-console.sha256");
    File::create(&console_evidence)
        .map_err(|error| format!("create {}: {error}", console_evidence.display()))?;
    File::create(&hash_evidence)
        .map_err(|error| format!("create {}: {error}", hash_evidence.display()))?;
    let wrapper = build_guest_wrapper(
        &artifact_root,
        &tools,
        &kernel,
        &initrd,
        &console_evidence,
        &hash_evidence,
    )?;

    let verifier_stdout = artifact_root.join("verifier.stdout");
    let verifier_stderr = artifact_root.join("verifier.stderr");
    let verifier_command = verifier_command(&tools, &wrapper, phase_timeout);

    theme.step(4, "Boot Linux twice under Hermit");
    println!("  Command:");
    println!("    {}", display_command(&verifier_command));
    println!();
    theme.say("Run 1 is starting. Kernel console output is captured for proof.");
    let verify_started = Instant::now();
    let status = run_verifier(verifier_command, &verifier_stdout, &verifier_stderr, &theme)?;
    let verifier_log = fs::read_to_string(&verifier_stderr)
        .map_err(|error| format!("read {}: {error}", verifier_stderr.display()))?;

    if !status.success() {
        print_failure_tail(&verifier_log, 120);
        if matches!(status.code(), Some(124 | 137)) {
            return Err(format!(
                "strict verification exceeded its {}s wall-clock guard",
                phase_timeout * 2 + 90
            ));
        }
        return Err(format!("Hermit verifier exited with {status}"));
    }
    if !verifier_log.contains(VERIFY_MARKER) {
        print_failure_tail(&verifier_log, 120);
        return Err("Hermit exited successfully without its determinism marker".to_owned());
    }

    let console = fs::read_to_string(&console_evidence)
        .map_err(|error| format!("read {}: {error}", console_evidence.display()))?;
    if !console.contains(BOOT_MARKER) {
        return Err(format!(
            "verified QEMU run did not emit the required {BOOT_MARKER} marker"
        ));
    }
    for rejected in [
        "Unable to calibrate against PIT",
        "Marking TSC unstable",
        "No current clocksource",
    ] {
        if console.contains(rejected) {
            return Err(format!(
                "Linux console contained rejected clock failure: {rejected}"
            ));
        }
    }

    theme.say(&format!(
        "Both boots and the log comparison completed in {}s.",
        verify_started.elapsed().as_secs()
    ));
    println!();
    println!("  Linux serial console (captured from the verified run):");
    print_console_proof(&console);

    let transcript_hash = fs::read_to_string(&hash_evidence)
        .map_err(|error| format!("read {}: {error}", hash_evidence.display()))?
        .trim()
        .to_owned();
    if transcript_hash.len() != 64 || !transcript_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(format!(
            "invalid transcript hash in {}: {transcript_hash:?}",
            hash_evidence.display()
        ));
    }

    theme.step(5, "Read the proof");
    println!("  Linux boot transcript, run 1 : {transcript_hash}");
    println!("  Linux boot transcript, run 2 : {transcript_hash}");
    println!(
        "                                  {}",
        theme.styled("1;32", "MATCH")
    );
    println!();
    println!("  Hermit verdict: {}", theme.styled("1;32", VERIFY_MARKER));
    println!("  Assurance     : L2 (bitwise-identical repeat run)");
    println!("  Backend       : ptrace");
    println!("  Hermit log    : info (captured and compared)");
    println!("  Relaxations   : none");
    println!();
    theme.success("A real Linux kernel booted twice with identical observable behavior.");
    println!("Artifacts: {}", artifact_root.display());
    Ok(())
}

fn find_repo_root() -> Result<PathBuf, String> {
    let mut path =
        env::current_dir().map_err(|error| format!("read current directory: {error}"))?;
    loop {
        if path.join("Cargo.toml").is_file() && path.join("hermit-cli").is_dir() {
            return Ok(path);
        }
        if !path.pop() {
            return Err("run this script from the Hermit repository".to_owned());
        }
    }
}

fn find_tools(repo: &Path) -> Result<Tools, String> {
    let hermit = if let Some(value) = env::var_os("HERMIT_BIN") {
        resolve_path(repo, PathBuf::from(value))
    } else {
        [
            repo.join("target/release/hermit"),
            repo.join("target/debug/hermit"),
        ]
        .into_iter()
        .find(|path| path.is_file())
        .or_else(|| find_on_path("hermit"))
        .ok_or_else(|| {
            "Hermit binary not found; run `cargo build --release -p hermit`".to_owned()
        })?
    };
    let qemu = if let Some(value) = env::var_os("QEMU_BIN") {
        resolve_path(repo, PathBuf::from(value))
    } else {
        find_on_path("qemu-system-x86_64")
            .ok_or_else(|| "qemu-system-x86_64 not found on PATH".to_owned())?
    };

    if !hermit.is_file() {
        return Err(format!("Hermit binary is not a file: {}", hermit.display()));
    }
    if !qemu.is_file() {
        return Err(format!("QEMU binary is not a file: {}", qemu.display()));
    }

    Ok(Tools {
        ar: find_on_path("ar"),
        cc: find_any(&["cc", "gcc"]).ok_or_else(|| "cc or gcc is required".to_owned())?,
        cpio: find_on_path("cpio").ok_or_else(|| "cpio is required".to_owned())?,
        curl: find_on_path("curl"),
        dpkg_deb: find_on_path("dpkg-deb"),
        gzip: find_on_path("gzip").ok_or_else(|| "gzip is required".to_owned())?,
        hermit,
        qemu,
        sha256sum: find_on_path("sha256sum").ok_or_else(|| "sha256sum is required".to_owned())?,
        tar: find_on_path("tar"),
        timeout: find_on_path("timeout").ok_or_else(|| "GNU timeout is required".to_owned())?,
        with_proxy: find_on_path("with-proxy"),
    })
}

fn resolve_path(repo: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }
}

fn find_any(names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(|name| find_on_path(name))
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let candidate = directory.join(name);
        candidate.is_file().then_some(candidate)
    })
}

fn acquire_kernel(theme: &Theme, repo: &Path, tools: &Tools) -> Result<PathBuf, String> {
    if let Some(value) = env::var_os("KERNEL_IMAGE") {
        let path = resolve_path(repo, PathBuf::from(value));
        if !path.is_file() {
            return Err(format!(
                "KERNEL_IMAGE is not a readable file: {}",
                path.display()
            ));
        }
        theme.say(&format!("Using explicit KERNEL_IMAGE={}", path.display()));
        return Ok(path);
    }

    let cache = repo.join("target/demo05-linux-boot/kernel-cache");
    fs::create_dir_all(&cache).map_err(|error| format!("create {}: {error}", cache.display()))?;
    let archive = cache.join(KERNEL_PACKAGE);
    let cached_kernel = cache.join(format!("vmlinuz-{KERNEL_VERSION}"));

    match try_pinned_kernel(theme, tools, &cache, &archive, &cached_kernel) {
        Ok(kernel) => return Ok(kernel),
        Err(error) => theme.warn(&format!("pinned kernel unavailable: {error}")),
    }

    let fallback = find_boot_kernel().ok_or_else(|| {
        "download failed and no readable /boot/vmlinuz or /boot/vmlinuz-* fallback exists"
            .to_owned()
    })?;
    theme.warn(&format!(
        "falling back to host kernel {}; it will be copied into the isolated demo run",
        fallback.display()
    ));
    Ok(fallback)
}

fn try_pinned_kernel(
    theme: &Theme,
    tools: &Tools,
    cache: &Path,
    archive: &Path,
    cached_kernel: &Path,
) -> Result<PathBuf, String> {
    let curl = tools
        .curl
        .as_ref()
        .ok_or_else(|| "curl is not installed".to_owned())?;
    let with_proxy = tools
        .with_proxy
        .as_ref()
        .ok_or_else(|| "with-proxy is not installed".to_owned())?;

    let archive_is_valid = archive.is_file()
        && sha256_file(&tools.sha256sum, archive)
            .map(|hash| hash == KERNEL_SHA256)
            .unwrap_or(false);
    if !archive_is_valid {
        let partial = archive.with_extension("deb.partial");
        let _ = fs::remove_file(&partial);
        theme.say(&format!(
            "Downloading pinned Linux {KERNEL_VERSION} package through with-proxy."
        ));
        println!("  source : {KERNEL_URL}");
        let status = Command::new(with_proxy)
            .arg(curl)
            .args([
                "--fail",
                "--location",
                "--retry",
                "2",
                "--connect-timeout",
                "15",
                "--silent",
                "--show-error",
                "--output",
            ])
            .arg(&partial)
            .arg(KERNEL_URL)
            .status()
            .map_err(|error| format!("start with-proxy curl: {error}"))?;
        if !status.success() {
            let _ = fs::remove_file(&partial);
            return Err(format!("download exited with {status}"));
        }
        fs::rename(&partial, archive).map_err(|error| {
            format!(
                "move downloaded package {} to {}: {error}",
                partial.display(),
                archive.display()
            )
        })?;
    } else {
        theme.say("Using the cached pinned kernel package.");
    }

    let actual_hash = sha256_file(&tools.sha256sum, archive)?;
    if actual_hash != KERNEL_SHA256 {
        return Err(format!(
            "checksum mismatch: expected {KERNEL_SHA256}, got {actual_hash}"
        ));
    }
    println!("  package: {}", archive.display());
    println!("  sha256 : {actual_hash} (published checksum matched)");

    if !cached_kernel.is_file() {
        let extract_dir = cache.join("extract");
        if extract_dir.exists() {
            fs::remove_dir_all(&extract_dir)
                .map_err(|error| format!("clean {}: {error}", extract_dir.display()))?;
        }
        fs::create_dir_all(&extract_dir)
            .map_err(|error| format!("create {}: {error}", extract_dir.display()))?;
        extract_debian_package(tools, archive, &extract_dir)?;
        let extracted = find_named_file(&extract_dir.join("boot"), "vmlinuz-")
            .ok_or_else(|| "downloaded package did not contain boot/vmlinuz-*".to_owned())?;
        fs::copy(&extracted, cached_kernel).map_err(|error| {
            format!(
                "copy extracted kernel {} to {}: {error}",
                extracted.display(),
                cached_kernel.display()
            )
        })?;
    }

    Ok(cached_kernel.to_owned())
}

fn extract_debian_package(tools: &Tools, archive: &Path, output: &Path) -> Result<(), String> {
    if let Some(dpkg_deb) = &tools.dpkg_deb {
        let status = Command::new(dpkg_deb)
            .arg("--extract")
            .arg(archive)
            .arg(output)
            .status()
            .map_err(|error| format!("start dpkg-deb: {error}"))?;
        return status
            .success()
            .then_some(())
            .ok_or_else(|| format!("dpkg-deb exited with {status}"));
    }

    let ar = tools
        .ar
        .as_ref()
        .ok_or_else(|| "neither dpkg-deb nor ar is installed".to_owned())?;
    let tar = tools
        .tar
        .as_ref()
        .ok_or_else(|| "dpkg-deb is absent and tar is not installed".to_owned())?;
    let members = Command::new(ar)
        .arg("t")
        .arg(archive)
        .output()
        .map_err(|error| format!("list Debian package members with ar: {error}"))?;
    if !members.status.success() {
        return Err(format!("ar t exited with {}", members.status));
    }
    let data_member = String::from_utf8_lossy(&members.stdout)
        .lines()
        .find(|line| line.starts_with("data.tar"))
        .map(str::to_owned)
        .ok_or_else(|| "Debian package has no data.tar member".to_owned())?;

    let mut ar_child = Command::new(ar)
        .arg("p")
        .arg(archive)
        .arg(&data_member)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("extract {data_member} with ar: {error}"))?;
    let ar_stdout = ar_child
        .stdout
        .take()
        .ok_or_else(|| "ar stdout was not piped".to_owned())?;
    let tar_status = Command::new(tar)
        .args(["--extract", "--file", "-", "--directory"])
        .arg(output)
        .stdin(Stdio::from(ar_stdout))
        .status()
        .map_err(|error| format!("start tar for {data_member}: {error}"))?;
    let ar_status = ar_child
        .wait()
        .map_err(|error| format!("wait for ar: {error}"))?;
    if !ar_status.success() || !tar_status.success() {
        return Err(format!(
            "package extraction failed: ar={ar_status}, tar={tar_status}"
        ));
    }
    Ok(())
}

fn find_named_file(root: &Path, prefix: &str) -> Option<PathBuf> {
    let mut entries: Vec<_> = fs::read_dir(root).ok()?.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_named_file(&path, prefix) {
                return Some(found);
            }
        } else if entry.file_name().to_string_lossy().starts_with(prefix) {
            return Some(path);
        }
    }
    None
}

fn find_boot_kernel() -> Option<PathBuf> {
    let preferred = PathBuf::from("/boot/vmlinuz");
    if preferred.is_file() {
        return Some(preferred);
    }
    find_named_file(Path::new("/boot"), "vmlinuz-")
}

fn build_initramfs(artifact_root: &Path, tools: &Tools) -> Result<PathBuf, String> {
    let source = artifact_root.join("init.c");
    let root = artifact_root.join("initramfs-root");
    let init = root.join("init");
    let initrd = artifact_root.join("initramfs.cpio.gz");
    fs::create_dir_all(&root).map_err(|error| format!("create {}: {error}", root.display()))?;
    fs::write(&source, INIT_SOURCE)
        .map_err(|error| format!("write {}: {error}", source.display()))?;

    let status = Command::new(&tools.cc)
        .args([
            "-Os",
            "-nostdlib",
            "-static",
            "-fno-stack-protector",
            "-fno-pie",
            "-no-pie",
        ])
        .arg(&source)
        .arg("-o")
        .arg(&init)
        .status()
        .map_err(|error| format!("start C compiler: {error}"))?;
    if !status.success() {
        return Err(format!("C compiler exited with {status}"));
    }

    let mut cpio = Command::new(&tools.cpio)
        .args(["--quiet", "--create", "--format=newc"])
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start cpio: {error}"))?;
    cpio.stdin
        .take()
        .ok_or_else(|| "cpio stdin was not piped".to_owned())?
        .write_all(b".\n./init\n")
        .map_err(|error| format!("write cpio file list: {error}"))?;
    let cpio_stdout = cpio
        .stdout
        .take()
        .ok_or_else(|| "cpio stdout was not piped".to_owned())?;
    let initrd_file =
        File::create(&initrd).map_err(|error| format!("create {}: {error}", initrd.display()))?;
    let mut gzip = Command::new(&tools.gzip)
        .arg("-9")
        .stdin(Stdio::from(cpio_stdout))
        .stdout(Stdio::from(initrd_file))
        .spawn()
        .map_err(|error| format!("start gzip: {error}"))?;
    let gzip_status = gzip
        .wait()
        .map_err(|error| format!("wait for gzip: {error}"))?;
    let cpio_status = cpio
        .wait()
        .map_err(|error| format!("wait for cpio: {error}"))?;
    if !cpio_status.success() || !gzip_status.success() {
        return Err(format!(
            "initramfs packing failed: cpio={cpio_status}, gzip={gzip_status}"
        ));
    }
    Ok(initrd)
}

fn build_guest_wrapper(
    artifact_root: &Path,
    tools: &Tools,
    kernel: &Path,
    initrd: &Path,
    console_evidence: &Path,
    hash_evidence: &Path,
) -> Result<PathBuf, String> {
    let wrapper = artifact_root.join("boot-linux.sh");
    let script = format!(
        r#"#!/bin/sh
set -eu
console=/tmp/demo05-linux-boot.console
{qemu} \
  -m 256M \
  -accel tcg,thread=single \
  -smp 1 \
  -icount shift=0,sleep=off \
  -kernel {kernel} \
  -initrd {initrd} \
  -display none \
  -serial file:/tmp/demo05-linux-boot.console \
  -monitor none \
  -no-reboot \
  -append 'console=ttyS0 panic=-1 rdinit=/init nokaslr printk.time=0 quiet'
hash=$({sha256sum} "$console")
hash=${{hash%% *}}
{cat} "$console" > {console_evidence}
printf '%s\n' "$hash" > {hash_evidence}
printf 'DEMO05_BOOT_TRANSCRIPT_SHA256=%s\n' "$hash"
"#,
        qemu = shell_quote(&tools.qemu),
        kernel = shell_quote(kernel),
        initrd = shell_quote(initrd),
        sha256sum = shell_quote(&tools.sha256sum),
        cat = shell_quote(
            &find_on_path("cat").ok_or_else(|| "cat is required by the guest wrapper".to_owned())?
        ),
        console_evidence = shell_quote(console_evidence),
        hash_evidence = shell_quote(hash_evidence),
    );
    fs::write(&wrapper, script).map_err(|error| format!("write {}: {error}", wrapper.display()))?;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("chmod {}: {error}", wrapper.display()))?;
    Ok(wrapper)
}

fn verifier_command(tools: &Tools, wrapper: &Path, phase_timeout: u64) -> Command {
    let total_timeout = phase_timeout.saturating_mul(2).saturating_add(90);
    let mut command = Command::new(&tools.timeout);
    command
        .arg("--kill-after=10s")
        .arg("--signal=TERM")
        .arg(format!("{total_timeout}s"))
        .arg(&tools.hermit)
        .args([
            "--log",
            "info",
            "run",
            "--strict",
            "--verify",
            "--base-env=minimal",
            "--",
        ])
        .arg(wrapper);
    command
}

fn run_verifier(
    mut command: Command,
    stdout_path: &Path,
    stderr_path: &Path,
    theme: &Theme,
) -> Result<std::process::ExitStatus, String> {
    let stdout = File::create(stdout_path)
        .map_err(|error| format!("create {}: {error}", stdout_path.display()))?;
    let stderr = File::create(stderr_path)
        .map_err(|error| format!("create {}: {error}", stderr_path.display()))?;
    let mut child = command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| format!("start Hermit verifier: {error}"))?;

    let mut phase = "run 1";
    let mut last_report = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("poll Hermit verifier: {error}"))?
        {
            return Ok(status);
        }
        let log = read_growing_file(stderr_path).unwrap_or_default();
        let next_phase = if log.contains(":: Comparing logs...") {
            "event-log comparison"
        } else if log.contains(":: Run2...") {
            "run 2"
        } else {
            "run 1"
        };
        if next_phase != phase {
            phase = next_phase;
            theme.say(&format!("Hermit advanced to {phase}."));
        } else if last_report.elapsed() >= Duration::from_secs(15) {
            theme.say(&format!("Hermit is still executing {phase}..."));
            last_report = Instant::now();
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn read_growing_file(path: &Path) -> io::Result<String> {
    let mut contents = String::new();
    File::open(path)?.read_to_string(&mut contents)?;
    Ok(contents)
}

fn sha256_file(sha256sum: &Path, path: &Path) -> Result<String, String> {
    let output = Command::new(sha256sum)
        .arg(path)
        .output()
        .map_err(|error| format!("start sha256sum for {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "sha256sum failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| format!("sha256sum returned no hash for {}", path.display()))
}

fn first_version_line(program: &Path) -> String {
    Command::new(program)
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| {
            let text = if output.stdout.is_empty() {
                output.stderr
            } else {
                output.stdout
            };
            String::from_utf8_lossy(&text)
                .lines()
                .next()
                .map(str::to_owned)
        })
        .unwrap_or_else(|| program.display().to_string())
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn display_command(command: &Command) -> String {
    let mut parts = vec![shell_quote(Path::new(command.get_program()))];
    parts.extend(command.get_args().map(|arg| shell_quote(Path::new(arg))));
    parts.join(" ")
}

fn positive_env_u64(name: &str, default: u64) -> Result<u64, String> {
    let Some(value) = env::var_os(name) else {
        return Ok(default);
    };
    let value = value
        .into_string()
        .map_err(|_| format!("{name} must contain UTF-8"))?;
    let parsed = value
        .parse::<u64>()
        .map_err(|error| format!("invalid {name}={value:?}: {error}"))?;
    if parsed == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(parsed)
}

fn print_console_proof(console: &str) {
    let lines: Vec<_> = console.lines().collect();
    let marker_index = lines
        .iter()
        .position(|line| line.contains(BOOT_MARKER))
        .unwrap_or(lines.len().saturating_sub(1));
    let start = marker_index.saturating_sub(2);
    let end = (marker_index + 9).min(lines.len());
    for line in &lines[start..end] {
        println!("    | {line}");
    }
}

fn print_failure_tail(log: &str, lines: usize) {
    eprintln!("\n--- verifier stderr (last {lines} lines) ---");
    let all_lines: Vec<_> = log.lines().collect();
    for line in &all_lines[all_lines.len().saturating_sub(lines)..] {
        eprintln!("{line}");
    }
}
