/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED

//! Execution-backend dispatch for `hermit run`.
//!
//! The DBI path launches the real guest through DynamoRIO and links the native
//! client against Hermit's `detcore-dbi` runtime. That runtime instantiates the
//! production [`detcore::Detcore`] Tool over [`reverie_dbi::DbiGuest`].

use std::ffi::OsStr;
use std::io::IsTerminal as _;
use std::io::Read;
use std::io::Seek as _;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::process::Command as StdCommand;
use std::process::Output;
use std::process::Stdio;

use hermit::Error;
use hermit::ExitStatus;
use reverie_dbi::DbiRunner;
use tracing::metadata::LevelFilter;

#[derive(Debug, Eq, PartialEq)]
struct DbiSummary {
    branches: u64,
    syscalls: u64,
    rewritten: u64,
    stdin_reads: u64,
    memory_hash: String,
}

const LITEINST_EVENT_PREFIX: &[u8] = b"reverie-liteinst: tool=compat syscall=";

#[derive(Debug, Eq, PartialEq)]
struct LiteinstObservation {
    guest_stderr: Vec<u8>,
    syscalls: Vec<i64>,
}

struct TeeReader<R, W> {
    input: R,
    replay: W,
}

impl<R: Read, W: Write> Read for TeeReader<R, W> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.input.read(buffer)?;
        self.replay.write_all(&buffer[..read])?;
        Ok(read)
    }
}

/// Runs `program` through DynamoRIO with the real Detcore Tool.
///
/// When `verify` is true, the guest is executed twice. Both runs must succeed,
/// produce byte-identical stdout, report `tool=Detcore`, and produce the same
/// observed guest-memory hash from the native DBI runtime.
pub fn run_dbi(
    program: &Path,
    args: &[String],
    verify: bool,
    log: Option<LevelFilter>,
) -> Result<ExitStatus, Error> {
    let stdin_is_terminal = std::io::stdin().is_terminal();

    let (drrun, client) = detcore_dbi::prepare_native_client().map_err(|error| {
        Error::msg(format!(
            "failed to prepare the Detcore DynamoRIO client: {error}"
        ))
    })?;
    let runner = DbiRunner::new(&drrun, &client)
        .map_err(|error| {
            Error::msg(format!(
                "failed to configure the DynamoRIO DBI runner (drrun={}, client={}): {error}",
                drrun.display(),
                client.display()
            ))
        })?
        .summary(true);

    eprintln!(
        "hermit: [dbi backend] Detcore Tool active; running {program:?} under DynamoRIO ({})",
        drrun.display()
    );

    let mut guest = StdCommand::new(program);
    if let Some(level) = log {
        guest.env("HERMIT_LOG", level.to_string());
    }
    guest.args(args);

    if !verify {
        if stdin_is_terminal {
            let status = runner
                .status(&guest)
                .map_err(|error| launch_error(&drrun, error))?;
            return Ok(process_status(status));
        }
        let output = run_once(&runner, &guest, &drrun, std::io::stdin())?;
        write_output(&output)?;
        return Ok(output_status(&output));
    }

    let mut replay = if stdin_is_terminal {
        None
    } else {
        Some(tempfile::tempfile()?)
    };

    eprintln!(":: DBI Run1...");
    let first = match replay.as_mut() {
        Some(replay) => {
            let first_input = TeeReader {
                input: std::io::stdin(),
                replay: replay.try_clone()?,
            };
            run_once(&runner, &guest, &drrun, first_input)?
        }
        None => run_once_with_terminal_input(&runner, &guest, &drrun)?,
    };
    if !first.status.success() {
        write_output(&first)?;
        return Ok(output_status(&first));
    }
    let first_summary = detcore_summary(&first)?;
    if stdin_is_terminal && first_summary.stdin_reads != 0 {
        write_output(&first)?;
        return Err(Error::msg(format!(
            "DBI verification cannot replay terminal stdin: guest attempted {} fd-0 read syscall(s)",
            first_summary.stdin_reads
        )));
    }

    eprintln!(":: DBI Run2...");
    let second = match replay.as_mut() {
        Some(replay) => {
            replay.seek(SeekFrom::Start(0))?;
            run_once(&runner, &guest, &drrun, replay.try_clone()?)?
        }
        None => run_once_with_terminal_input(&runner, &guest, &drrun)?,
    };
    if !second.status.success() {
        write_output(&second)?;
        return Ok(output_status(&second));
    }
    let second_summary = detcore_summary(&second)?;

    if first.stdout != second.stdout {
        return Err(Error::msg(
            "DBI verification failed: guest stdout differed between runs",
        ));
    }
    if first_summary != second_summary {
        return Err(Error::msg(format!(
            "DBI verification failed: native Detcore summaries differed ({first_summary:?} != {second_summary:?})"
        )));
    }

    write_output(&first)?;
    eprintln!(
        ":: Comparing DBI observed guest-memory hashes... {} | {}",
        first_summary.memory_hash, second_summary.memory_hash
    );
    eprintln!(":: DBI path confirmed: DynamoRIO client reported tool=Detcore");
    eprintln!(":: Success: deterministic. Determinism verified.");
    Ok(ExitStatus::Exited(0))
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#585): Review the experimental preload verification boundary.
/// Runs a guest through the LiteInst preload compatibility path.
pub fn run_liteinst(
    program: &Path,
    args: &[String],
    verify: bool,
    log: Option<LevelFilter>,
) -> Result<ExitStatus, Error> {
    eprintln!(
        "hermit: [liteinst backend] experimental preload compatibility mode; Detcore Tool is not active"
    );

    let stdin_is_terminal = std::io::stdin().is_terminal();
    let mut replay = if stdin_is_terminal {
        None
    } else {
        let mut replay = tempfile::tempfile()?;
        let mut stdin = std::io::stdin().lock();
        std::io::copy(&mut stdin, &mut replay)?;
        replay.seek(SeekFrom::Start(0))?;
        Some(replay)
    };

    if verify {
        eprintln!(":: LiteInst Run1...");
    }
    let first = run_liteinst_once(program, args, log, replay.as_ref())?;
    let first_observation = liteinst_observation(&first)?;
    if !first.status.success() || !verify {
        write_liteinst_output(&first, &first_observation)?;
        eprintln!(
            ":: LiteInst path confirmed: preload reported {} stable syscall events",
            first_observation.syscalls.len()
        );
        return Ok(output_status(&first));
    }

    if let Some(replay) = replay.as_mut() {
        replay.seek(SeekFrom::Start(0))?;
    }
    eprintln!(":: LiteInst Run2...");
    let second = run_liteinst_once(program, args, log, replay.as_ref())?;
    let second_observation = liteinst_observation(&second)?;
    if !second.status.success() {
        write_liteinst_output(&second, &second_observation)?;
        return Ok(output_status(&second));
    }

    if first.stdout != second.stdout {
        return Err(Error::msg(
            "LiteInst verification failed: guest stdout differed between runs",
        ));
    }
    if first_observation.guest_stderr != second_observation.guest_stderr {
        return Err(Error::msg(
            "LiteInst verification failed: guest stderr differed between runs",
        ));
    }
    if first_observation.syscalls != second_observation.syscalls {
        return Err(Error::msg(format!(
            "LiteInst verification failed: syscall sequences differed ({} events != {} events)",
            first_observation.syscalls.len(),
            second_observation.syscalls.len()
        )));
    }

    write_liteinst_output(&first, &first_observation)?;
    eprintln!(
        ":: Comparing LiteInst syscall sequences... {} | {}",
        first_observation.syscalls.len(),
        second_observation.syscalls.len()
    );
    eprintln!(":: LiteInst path confirmed: preload compatibility events matched");
    eprintln!(":: Success: deterministic. Determinism verified.");
    Ok(ExitStatus::Exited(0))
}

fn run_liteinst_once(
    program: &Path,
    args: &[String],
    log: Option<LevelFilter>,
    input: Option<&std::fs::File>,
) -> Result<Output, Error> {
    let mut guest = StdCommand::new(program);
    guest.args(args);
    if let Some(level) = log {
        guest.env("HERMIT_LOG", level.to_string());
    }
    configure_liteinst_guest(&mut guest)?;
    match input {
        Some(input) => {
            guest.stdin(Stdio::from(input.try_clone()?));
        }
        None => {
            guest.stdin(Stdio::inherit());
        }
    }
    guest
        .output()
        .map_err(|error| Error::msg(format!("failed to launch LiteInst guest: {error}")))
}

fn configure_liteinst_guest(command: &mut StdCommand) -> Result<(), Error> {
    let mut preload = hermit::liteinst_runtime_library_path()?.into_os_string();
    if let Some(existing) = std::env::var_os("LD_PRELOAD").filter(|value| !value.is_empty()) {
        preload.push(OsStr::new(":"));
        preload.push(existing);
    }
    command
        .env("LD_PRELOAD", preload)
        .env("REVERIE_LITEINST_TOOL", "compat");
    Ok(())
}

fn liteinst_observation(output: &Output) -> Result<LiteinstObservation, Error> {
    let mut observation = LiteinstObservation {
        guest_stderr: Vec::new(),
        syscalls: Vec::new(),
    };
    for line in output.stderr.split_inclusive(|byte| *byte == b'\n') {
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        if let Some(number) = content.strip_prefix(LITEINST_EVENT_PREFIX) {
            let number = std::str::from_utf8(number)
                .map_err(|_| Error::msg("LiteInst emitted a non-UTF-8 syscall number"))?
                .parse::<i64>()
                .map_err(|_| Error::msg("LiteInst emitted an invalid syscall number"))?;
            observation.syscalls.push(number);
        } else {
            observation.guest_stderr.extend_from_slice(line);
        }
    }
    if observation.syscalls.is_empty() {
        return Err(Error::msg(
            "LiteInst verification failed: preload emitted no compatibility events",
        ));
    }
    Ok(observation)
}

fn write_liteinst_output(output: &Output, observation: &LiteinstObservation) -> Result<(), Error> {
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&observation.guest_stderr)?;
    Ok(())
}

fn run_once<R: Read + Send>(
    runner: &DbiRunner,
    guest: &StdCommand,
    drrun: &Path,
    input: R,
) -> Result<Output, Error> {
    runner
        .output_with_reader(guest, input)
        .map_err(|error| launch_error(drrun, error))
}

fn run_once_with_terminal_input(
    runner: &DbiRunner,
    guest: &StdCommand,
    drrun: &Path,
) -> Result<Output, Error> {
    runner
        .output_with_inherited_stdin(guest)
        .map_err(|error| launch_error(drrun, error))
}

fn launch_error(drrun: &Path, error: std::io::Error) -> Error {
    Error::msg(format!(
        "failed to launch drrun ({}): {error}",
        drrun.display()
    ))
}

fn process_status(status: std::process::ExitStatus) -> ExitStatus {
    ExitStatus::Exited(status.code().unwrap_or(1))
}

fn detcore_summary(output: &Output) -> Result<DbiSummary, Error> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let summary = stderr
        .lines()
        .rev()
        .find(|line| line.starts_with("reverie-dbi: tool=Detcore "))
        .ok_or_else(|| {
            Error::msg(
                "DBI verification failed: native DynamoRIO summary did not report tool=Detcore",
            )
        })?;

    let field = |name: &str| {
        summary
            .split_ascii_whitespace()
            .find_map(|value| value.strip_prefix(name))
            .ok_or_else(|| Error::msg(format!("DBI verification failed: summary omitted {name}")))
    };
    let branches = field("branches=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBI verification failed: invalid branch count"))?;
    let syscalls = field("syscalls=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBI verification failed: invalid syscall count"))?;
    let rewritten = field("rewritten=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBI verification failed: invalid rewritten count"))?;
    let stdin_reads = field("stdin_reads=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBI verification failed: invalid stdin read count"))?;
    if branches == 0 || syscalls == 0 || rewritten == 0 || rewritten > syscalls {
        return Err(Error::msg(
            "DBI verification failed: native callback counters are inconsistent",
        ));
    }

    let hash = field("memory_hash=")?;
    if hash.len() != 16 || u64::from_str_radix(hash, 16).is_err() {
        return Err(Error::msg(
            "DBI verification failed: invalid observed-memory hash",
        ));
    }
    Ok(DbiSummary {
        branches,
        syscalls,
        rewritten,
        stdin_reads,
        memory_hash: hash.to_owned(),
    })
}

fn write_output(output: &Output) -> Result<(), Error> {
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    Ok(())
}

fn output_status(output: &Output) -> ExitStatus {
    ExitStatus::Exited(output.status.code().unwrap_or(1))
}
