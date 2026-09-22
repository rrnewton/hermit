/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! OPT-IN DIAGNOSTIC. NEVER LANDS. Disposable-clone only.
//!
//! Records the entry and exit stops of the `process_vm_readv` that produces
//! Detcore's DETLOG `[memory] ... [stack]` digest input under the SaBRe
//! backend, so the bytes that digest covers can be attributed to an owner.
//!
//! WHY THIS LIVES IN THE SUPERVISOR AND NOT IN DETCORE
//!   Under SaBRe the Detcore plugin executes ON the guest stack, inside the
//!   very `[stack]` VMA being hashed. Any recording code placed in the guest
//!   would push frames into that VMA. For a recording taken BEFORE the digest
//!   input is read, that would contaminate the measurement. This module runs in
//!   the hermit supervisor process instead. It only reads tracee memory
//!   (`process_vm_readv`, supervisor -> guest) and never writes it, so neither
//!   the entry-stop nor the exit-stop recording puts a single byte into the
//!   measured range.
//!
//! WHAT IT DOES NOT DO
//!   It does not change what the digest covers, does not exclude any byte, and
//!   does not touch detcore, procmaps, the digest, any fixture, or the
//!   manifest. Every entry point is a no-op unless the opt-in environment
//!   variable is set.

use std::collections::HashMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;

/// x86-64 `process_vm_readv`.
const SYS_PROCESS_VM_READV: u64 = 310;

/// Only whole-VMA reads get full byte dumps. Smaller in-range reads (syscall
/// argument marshalling, for example) still get a manifest row, so nothing is
/// silently dropped -- the manifest states which rows were dumped.
const FULL_DUMP_MIN: u64 = 0x10000;

const DIR_ENV: &str = "HERMIT_DIAG_STACK_CAPTURE_DIR";
const RANGE_ENV: &str = "HERMIT_DIAG_STACK_CAPTURE_RANGE";

struct Config {
    dir: PathBuf,
    lo: u64,
    hi: u64,
    manifest: Mutex<File>,
}

struct Pending {
    seq: u32,
    pid_arg: u64,
    remote_base: u64,
    remote_len: u64,
    local_base: u64,
    entry_rsp: u64,
    entry_rip: u64,
}

static CONFIG: OnceLock<Option<Config>> = OnceLock::new();
static STATE: OnceLock<Mutex<HashMap<i32, Pending>>> = OnceLock::new();
/// Global, never-resetting dump id. Per-pid counters collide because the two
/// `--verify` runs reuse the same namespaced guest pid, which would silently
/// overwrite run 1's dumps with run 2's.
static DUMP_ID: AtomicU32 = AtomicU32::new(0);

fn parse_u64(text: &str) -> Option<u64> {
    let text = text.trim();
    let text = text.strip_prefix("0x").unwrap_or(text);
    u64::from_str_radix(text, 16).ok()
}

fn config() -> Option<&'static Config> {
    CONFIG
        .get_or_init(|| {
            let dir = PathBuf::from(std::env::var_os(DIR_ENV)?);
            let range = std::env::var(RANGE_ENV).ok()?;
            let (lo, hi) = range.split_once('-')?;
            let lo = parse_u64(lo)?;
            let hi = parse_u64(hi)?;
            std::fs::create_dir_all(&dir).ok()?;
            // Each `--verify` run is a SEPARATE supervisor process, and both run
            // inside a PID namespace where the supervisor is pid 1 and the guest
            // is pid 3. A per-process counter or a pid-derived name therefore
            // COLLIDES across the two runs, and run 2 silently overwrites run 1's
            // dumps -- observed directly: 260 whole-VMA exit rows but only 130
            // files on disk. Claim a private numbered subdirectory instead;
            // create_dir fails if it already exists, so the claim is atomic.
            let mut dir = dir;
            for index in 0..1024 {
                let candidate = dir.join(format!("proc-{index}"));
                if std::fs::create_dir(&candidate).is_ok() {
                    dir = candidate;
                    break;
                }
            }
            let mut manifest = OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("manifest.tsv"))
                .ok()?;
            let _ = writeln!(
                manifest,
                "supervisor_pid\ttracee\tseq\tstop\tpid_arg\tremote_base\tremote_len\t\
                 local_base\tret\tentry_rsp\tentry_rip\texit_rsp\texit_rip\tdumped\t\
                 fs_base\ttls_canary\ttls_ptr_guard"
            );
            let _ = manifest.flush();
            Some(Config {
                dir,
                lo,
                hi,
                manifest: Mutex::new(manifest),
            })
        })
        .as_ref()
}

fn state() -> &'static Mutex<HashMap<i32, Pending>> {
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Read `len` bytes at `addr` out of tracee `pid`. Supervisor -> guest only.
/// There is deliberately no write counterpart in this module.
fn peek(pid: i32, addr: u64, len: usize) -> Option<Vec<u8>> {
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    let local = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: len,
    };
    let remote = libc::iovec {
        iov_base: addr as *mut libc::c_void,
        iov_len: len,
    };
    let n = unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(buf)
}

fn dump(cfg: &Config, pid: i32, seq: u32, what: &str, addr: u64, len: usize) -> usize {
    match peek(pid, addr, len) {
        Some(bytes) => {
            let path = cfg.dir.join(format!("p{pid}_r{seq:03}_{what}.bin"));
            match std::fs::write(&path, &bytes) {
                Ok(()) => bytes.len(),
                Err(_) => 0,
            }
        }
        None => 0,
    }
}

fn read_iovec(pid: i32, addr: u64) -> Option<(u64, u64)> {
    let bytes = peek(pid, addr, 16)?;
    if bytes.len() < 16 {
        return None;
    }
    let base = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
    let len = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
    Some((base, len))
}

/// x86-64 `getrandom`.
const SYS_GETRANDOM: u64 = 318;

/// Pending getrandom(buf, len) recorded at entry, read back at exit.
static RANDOM_STATE: OnceLock<Mutex<HashMap<i32, (u64, u64, u64)>>> = OnceLock::new();

fn random_state() -> &'static Mutex<HashMap<i32, (u64, u64, u64)>> {
    RANDOM_STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Records the bytes every `getrandom` in the guest process actually returned,
/// with the return address, so a run-varying value found on the stack can be
/// traced to the call that produced it instead of guessed at.
fn note_getrandom_entry(pid: i32, regs: &libc::user_regs_struct) {
    if regs.orig_rax != SYS_GETRANDOM {
        return;
    }
    random_state()
        .lock()
        .unwrap()
        .insert(pid, (regs.rdi, regs.rsi, regs.rip));
}

fn note_getrandom_exit(cfg: &Config, pid: i32, regs: &libc::user_regs_struct) {
    let Some((buf, len, rip)) = random_state().lock().unwrap().remove(&pid) else {
        return;
    };
    let ret = regs.rax as i64;
    let bytes = if ret > 0 {
        peek(pid, buf, ret as usize).unwrap_or_default()
    } else {
        Vec::new()
    };
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.dir.join("getrandom.tsv"))
    {
        let _ = writeln!(file, "{pid}\t{buf:#x}\t{len}\t{ret}\t{rip:#x}\t{hex}");
    }
}

/// Called at every guest syscall-entry stop the supervisor already takes.
pub fn on_syscall_entry(pid: i32, regs: &libc::user_regs_struct) {
    let Some(cfg) = config() else { return };
    note_getrandom_entry(pid, regs);
    if regs.orig_rax != SYS_PROCESS_VM_READV {
        return;
    }
    // process_vm_readv(pid, local_iov, liovcnt, remote_iov, riovcnt, flags)
    let (pid_arg, liov_p, liovcnt, riov_p, riovcnt) =
        (regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8);
    if liovcnt < 1 || riovcnt < 1 {
        return;
    }
    let Some((remote_base, remote_len)) = read_iovec(pid, riov_p) else {
        return;
    };
    let Some((local_base, _local_len)) = read_iovec(pid, liov_p) else {
        return;
    };
    if remote_base < cfg.lo || remote_base >= cfg.hi {
        return;
    }

    let seq_now = DUMP_ID.fetch_add(1, Ordering::SeqCst) + 1;

    let dumped = if remote_len >= FULL_DUMP_MIN {
        dump(
            cfg,
            pid,
            seq_now,
            "srcentry",
            cfg.lo,
            (cfg.hi - cfg.lo) as usize,
        )
    } else {
        0
    };

    if let Ok(mut manifest) = cfg.manifest.lock() {
        let _ = writeln!(
            manifest,
            "{}\t{pid}\t{seq_now}\tentry\t{pid_arg}\t{remote_base:#x}\t{remote_len}\t\
             {local_base:#x}\t-\t{:#x}\t{:#x}\t-\t-\t{dumped}\t{:#x}\t{}\t{}",
            std::process::id(),
            regs.rsp,
            regs.rip,
            regs.fs_base,
            // glibc keeps the stack-protector guard at TCB offset 0x28. Reading
            // it identifies the canary for THIS process, instead of inferring
            // one from a value that merely looks like a canary.
            match peek(pid, regs.fs_base.wrapping_add(0x28), 8) {
                Some(v) if v.len() == 8 =>
                    format!("{:#018x}", u64::from_le_bytes(v.try_into().unwrap())),
                _ => "unreadable".to_string(),
            },
            // glibc keeps the PTR_MANGLE pointer guard at TCB offset 0x30.
            match peek(pid, regs.fs_base.wrapping_add(0x30), 8) {
                Some(v) if v.len() == 8 =>
                    format!("{:#018x}", u64::from_le_bytes(v.try_into().unwrap())),
                _ => "unreadable".to_string(),
            }
        );
        let _ = manifest.flush();
    }

    state().lock().unwrap().insert(
        pid,
        Pending {
            seq: seq_now,
            pid_arg,
            remote_base,
            remote_len,
            local_base,
            entry_rsp: regs.rsp,
            entry_rip: regs.rip,
        },
    );
}

/// Called at every guest syscall-exit stop the supervisor already takes.
pub fn on_syscall_exit(pid: i32, regs: &libc::user_regs_struct) {
    let Some(cfg) = config() else { return };
    note_getrandom_exit(cfg, pid, regs);
    let Some(pending) = state().lock().unwrap().remove(&pid) else {
        return;
    };
    let ret = regs.rax as i64;

    let (copy_bytes, srcexit_bytes) = if pending.remote_len >= FULL_DUMP_MIN {
        let copy = if ret > 0 {
            dump(cfg, pid, pending.seq, "copy", pending.local_base, ret as usize)
        } else {
            0
        };
        let srcexit = dump(
            cfg,
            pid,
            pending.seq,
            "srcexit",
            cfg.lo,
            (cfg.hi - cfg.lo) as usize,
        );
        (copy, srcexit)
    } else {
        (0, 0)
    };

    if let Ok(mut manifest) = cfg.manifest.lock() {
        let _ = writeln!(
            manifest,
            "{}\t{pid}\t{}\texit\t{}\t{:#x}\t{}\t{:#x}\t{ret}\t{:#x}\t{:#x}\t{:#x}\t{:#x}\t\
             copy={copy_bytes},srcexit={srcexit_bytes}",
            std::process::id(),
            pending.seq,
            pending.pid_arg,
            pending.remote_base,
            pending.remote_len,
            pending.local_base,
            pending.entry_rsp,
            pending.entry_rip,
            regs.rsp,
            regs.rip
        );
        let _ = manifest.flush();
    }
}
