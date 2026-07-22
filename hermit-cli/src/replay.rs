/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use reverie::ExitStatus;
use reverie::process::Command;
use reverie::process::Mount;
use reverie::process::Output;
use reverie::process::Stdio;

use crate::Shebang;
use crate::chroot::TempChroot;
use crate::consts::EXE_NAME;
use crate::consts::EXECUTABLES_NAME;
use crate::consts::METADATA_NAME;
use crate::error::Context;
use crate::error::Error;
use crate::interp;
use crate::metadata::Metadata;
use crate::metadata::RECORD_VERSION;
use crate::metadata::record_or_replay_config;
use crate::replayer::Replayer;

type ReplayTool = detcore::Detcore<Replayer>;
type Tracer = reverie_ptrace::Tracer<detcore::GlobalState>;

/// Represents a replay that is currently running.
pub struct Replay {
    // The running tracee.
    tracer: Tracer,

    // The chroot. When dropped, everything in this directory will be
    // recursively deleted.
    chroot: TempChroot,
}

impl Replay {
    /// Spawns a new replay using the provided base directory where the replay
    /// data is stored.
    pub async fn spawn(
        dir: &Path,
        capture_output: bool,
        gdbserver: Option<u16>,
    ) -> Result<Self, Error> {
        let metadata_path = dir.join(METADATA_NAME);

        let metadata: Metadata = serde_json::from_reader(
            fs::File::open(&metadata_path)
                .with_context(|| format!("Failed to open {:?}", metadata_path))?,
        )
        .with_context(|| format!("Failed to parse {:?}", metadata_path))?;

        let recording_version = &metadata.version;
        let replayer_version = &RECORD_VERSION;
        if !replayer_version.compatible_with(recording_version) {
            return Err(anyhow::anyhow!(format!(
                "Version mismatch, recording version {:?}, replayer version {:?}",
                recording_version, replayer_version
            )));
        }

        let mut command = metadata.command();

        if capture_output {
            command.stdin(Stdio::null());
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());
        }

        let config = record_or_replay_config(dir);
        let sequentialize_threads = config.sequentialize_threads;

        let chroot =
            prepare_chroot(dir, &metadata).context("Failed to create chroot environment")?;

        // bind mount fbcode otherwise many program can fail to execve due to missing
        // shared libraries. This path only exists on Meta hosts; skip it elsewhere
        // (e.g. generic self-hosted CI runners) where the missing source would make
        // mount(2) fail with ENOENT.
        let fbcode = Path::new("/usr/local/fbcode");
        if fbcode.exists() {
            command.mount(
                Mount::bind(fbcode, chroot.path().join("usr/local/fbcode"))
                    .recursive()
                    .touch_target(),
            );
        }

        command.chroot(chroot.path());

        let mut builder = reverie_ptrace::TracerBuilder::<ReplayTool>::new(command).config(config);
        if let Some(port) = gdbserver {
            builder = builder.gdbserver(port);
        }
        if sequentialize_threads {
            // Inform gdbserver not to serialize guests because this is
            // done by detcore already.
            builder = builder.sequentialized_guest();
        }
        let tracer = builder.spawn().await?;

        Ok(Self { tracer, chroot })
    }

    /// Waits for the replay to finish and returns its exit status.
    pub async fn wait(self) -> Result<ExitStatus, reverie::Error> {
        let (exit_status, global_state) = self.tracer.wait().await?;
        self.chroot.remove()?;
        global_state.clean_up(false, &None).await;
        Ok(exit_status)
    }

    /// Waits for the replay to finish and collects its output.
    pub async fn wait_with_output(self) -> Result<Output, reverie::Error> {
        let (output, global_state) = self.tracer.wait_with_output().await?;
        self.chroot.remove()?;
        global_state.clean_up(false, &None).await;
        Ok(output)
    }
}

/// Creates the temporary chroot directory.
fn prepare_chroot(dir: &Path, metadata: &Metadata) -> io::Result<TempChroot> {
    let chroot = TempChroot::new_in(dir)?;

    let exe = dir.join(EXE_NAME);

    // Hard link the executable. Hard linking is okay here since the chroot
    // directory and the executable live on the same file system. The executable
    // is also unlikely to be modified during the program's lifetime.
    chroot.hard_link(&exe, &metadata.exe)?;
    if let Some(shebang) = Shebang::new(&metadata.exe) {
        chroot.copy_same(shebang.interpreter())?;
        // check if shebang is wrapped as #! /usr/bin/env <program>, in that
        // case, copy both /usr/bin/env and <program> (resolved)
        if let Some(program) = shebang.args().next() {
            // copy 2nd interpreter iff it is a valid program.
            if let Ok(program) = Command::new(program).find_program() {
                chroot.copy_same(&program)?;
            }
        }

        if let Ok(python3) = fs::read_link("/usr/local/bin/python3") {
            chroot.symlink(&python3, Path::new("/usr/local/bin/python3"))?;
        }
    }

    let default_ldso = Path::new("/lib64/ld-linux-x86-64.so.2");
    // FIXME: ld.so is copied over from the host system, but it really should be
    // recorded correctly.
    //
    // There are a few ways to find the path to `ld.so`.
    //  1. Parse the ELF. The path to `ld.so` can be found in the "INTERP"
    //     program header. (See `readelf -l /usr/bin/ls`.)
    //  2. Use the `AT_BASE` auxval to find the starting address of the
    //     interpreter. Then, use this to find which memory map it is associated
    //     with in `/proc/{pid}/maps`. This is the method used by RR.
    //  3. Use the `AT_PHDR`, `AT_PHNUM`, and `AT_PHENT` auxvals to read the
    //     program headers until reaching the `INTERP` program header.
    chroot.copy_same(default_ldso)?;

    if let Some(interp) = interp::elf_get_interp(&metadata.exe)
        && interp.is_file()
        && interp != default_ldso
    {
        chroot.copy_same(&interp)?;
    }

    // Stage every executable that the recording `execve`'d, not just the root
    // program. A pipeline or any program that spawns children (e.g.
    // `bash -c "… | wc -l"`) execs binaries beyond `metadata.exe`. Those files
    // must exist inside the chroot for the child `execve` to succeed during
    // replay; otherwise it fails with ENOENT, the guest takes a different code
    // path than it did while recording, and replay desyncs from the recorded
    // event stream.
    //
    // FIXME: Like ld.so above, these binaries are copied from the host rather
    // than reconstructed from the recording. They should eventually be recorded
    // so that replay does not depend on host filesystem state.
    stage_recorded_executables(dir, metadata, &chroot);

    // Create the working directory.
    chroot.create_dir_all(&metadata.current_dir)?;

    Ok(chroot)
}

/// Copies every executable listed in the recording's `executables` manifest into
/// the chroot, along with each one's ELF interpreter (`ld.so`). The dynamic
/// linker's own file operations are replayed from the recording, so only the
/// executable and its interpreter files themselves need to exist on disk.
///
/// This is best-effort: any path that is missing, unreadable, or already present
/// (the root executable is hard-linked earlier) is skipped rather than failing
/// the replay, so that a single odd exec target cannot block an otherwise valid
/// replay.
fn stage_recorded_executables(dir: &Path, metadata: &Metadata, chroot: &TempChroot) {
    let manifest = dir.join(EXECUTABLES_NAME);
    let contents = match fs::read(&manifest) {
        Ok(contents) => contents,
        // No manifest means the recording predates executable tracking (or
        // recorded no execs); nothing to stage.
        Err(_) => return,
    };

    for line in contents.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }

        let path = Path::new(OsStr::from_bytes(line));
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            metadata.current_dir.join(path)
        };

        // The root executable is already hard-linked into the chroot; don't
        // clobber it (or re-copy any binary we've already staged).
        if chroot.relpath(&resolved).exists() {
            continue;
        }

        if !resolved.is_file() {
            continue;
        }

        if let Err(err) = chroot.copy_same(&resolved) {
            tracing::warn!(
                "Failed to stage executable {:?} into replay chroot: {}",
                resolved,
                err
            );
            continue;
        }

        // The kernel needs this binary's dynamic linker present to exec it.
        if let Some(interp) = interp::elf_get_interp(&resolved)
            && interp.is_file()
            && !chroot.relpath(&interp).exists()
            && let Err(err) = chroot.copy_same(&interp)
        {
            tracing::warn!(
                "Failed to stage interpreter {:?} for {:?} into replay chroot: {}",
                interp,
                resolved,
                err
            );
        }
    }
}
