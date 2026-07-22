/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Execution-backend selection for `hermit run`.
//!
//! Hermit's production backend is `reverie-ptrace`, which runs an arbitrary ELF
//! guest under seccomp + ptrace. Two experimental Reverie backends are also
//! wired here so the same Detcore/Reverie contracts can be exercised over
//! alternative execution mechanisms:
//!
//! * [`Backend::Dbi`] — in-process DynamoRIO instrumentation (`reverie-dbi`).
//! * [`Backend::Kvm`] — a small KVM guest (`reverie-kvm`).
//!
//! Both are prototypes and do not yet load and execute arbitrary Linux ELF
//! programs the way the ptrace backend does (see each crate's README). To keep
//! `hermit run --backend {dbi,kvm}` useful for kicking the tires, they run a
//! minimal "hello world" demonstration through their real interception path.

use std::path::Path;
use std::process::Command as StdCommand;

use clap::ValueEnum;
use hermit::Error;
use hermit::ExitStatus;

/// The execution backend used by `hermit run`.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[clap(rename_all = "lower")]
pub enum Backend {
    /// Production backend: seccomp + ptrace, runs arbitrary ELF guests.
    #[default]
    Ptrace,
    /// Experimental in-process DynamoRIO backend (`reverie-dbi`).
    Dbi,
    /// Experimental KVM backend (`reverie-kvm`).
    Kvm,
}

/// Runs a "hello world" through the experimental KVM backend.
///
/// `reverie-kvm` is not yet a Linux ELF execution backend, so this cannot exec
/// `program`. Instead it builds a tiny real-mode guest that issues a single
/// `write` syscall via `vmcall`; the host handler performs the actual write, so
/// the message reaches the real stdout. This exercises the genuine
/// VM-exit → Reverie syscall interception path (with the deterministic CPUID
/// policy applied to the vCPU).
pub fn run_kvm(program: &Path) -> Result<ExitStatus, Error> {
    use reverie_kvm::KvmBackend;
    use reverie_kvm::SyscallRequest;

    const MEMORY_SIZE: usize = 0x1_0000;
    const ENTRY_POINT: u64 = 0x1000;
    const FRAME_ADDRESS: u64 = 0x2000;
    const MESSAGE_ADDRESS: u64 = 0x3000;

    eprintln!(
        "hermit: [kvm backend] {program:?} is not executed as an ELF; the reverie-kvm prototype \
         runs a built-in hello-world guest that issues write(2) via vmcall."
    );

    let message = b"hello world\n";

    let mut backend = KvmBackend::new(MEMORY_SIZE)
        .map_err(|e| Error::msg(format!("failed to create KVM backend (need /dev/kvm): {e}")))?;
    backend
        .memory_mut()
        .write(MESSAGE_ADDRESS, message)
        .map_err(|e| Error::msg(format!("failed to stage guest message: {e}")))?;
    backend
        .install_syscall(
            ENTRY_POINT,
            FRAME_ADDRESS,
            SyscallRequest::new(
                libc::SYS_write as u64,
                [1, MESSAGE_ADDRESS, message.len() as u64, 0, 0, 0],
            ),
        )
        .map_err(|e| Error::msg(format!("failed to install guest syscall: {e}")))?;

    let mut result: i64 = -1;
    backend
        .run(|request, memory| {
            // Perform the intercepted write on the host so its bytes reach the
            // real fd, mirroring what a full backend's Tool would delegate.
            let len = request.args()[2] as usize;
            let mut buf = vec![0u8; len];
            if memory.read(request.args()[1], &mut buf).is_err() {
                return -(libc::EFAULT as i64);
            }
            let fd = request.args()[0] as i32;
            let written =
                unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, len) } as i64;
            result = written;
            written
        })
        .map_err(|e| Error::msg(format!("KVM guest run failed: {e}")))?;

    if result < 0 {
        return Ok(ExitStatus::Exited(1));
    }
    Ok(ExitStatus::Exited(0))
}

/// Runs `program`/`args` through the experimental DynamoRIO backend.
///
/// `reverie-dbi`'s native client is built and launched by DynamoRIO's own
/// toolchain (it is intentionally outside Cargo because DynamoRIO's CMake
/// package supplies the required client linker flags). This shells out to
/// `drrun` with the prebuilt client. Configure it with two env vars:
///
/// * `HERMIT_DRRUN` — path to DynamoRIO's `drrun`.
/// * `HERMIT_DBI_CLIENT` — path to `libreverie_dbi_client.so`.
pub fn run_dbi(program: &Path, args: &[String]) -> Result<ExitStatus, Error> {
    let drrun = std::env::var("HERMIT_DRRUN").map_err(|_| {
        Error::msg(
            "the dbi backend needs the DynamoRIO SDK. Set HERMIT_DRRUN=<dynamorio>/bin64/drrun and \
             HERMIT_DBI_CLIENT=<...>/libreverie_dbi_client.so (build the client with \
             reverie-dbi/scripts/build-client.sh). See reverie-dbi/README.md.",
        )
    })?;
    let client = std::env::var("HERMIT_DBI_CLIENT").map_err(|_| {
        Error::msg("the dbi backend needs HERMIT_DBI_CLIENT=<...>/libreverie_dbi_client.so")
    })?;

    eprintln!("hermit: [dbi backend] running {program:?} under DynamoRIO ({drrun})");

    let status = StdCommand::new(&drrun)
        .arg("-disable_rseq")
        .arg("-c")
        .arg(&client)
        .arg("--")
        .arg(program)
        .args(args)
        .status()
        .map_err(|e| Error::msg(format!("failed to launch drrun ({drrun}): {e}")))?;

    Ok(ExitStatus::Exited(status.code().unwrap_or(1)))
}
