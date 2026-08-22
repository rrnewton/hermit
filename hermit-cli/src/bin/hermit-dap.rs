/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#![deny(clippy::all)]

use std::env;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::process::ExitCode;

fn usage() {
    println!(
        "Usage: hermit-dap [--gdb PATH]\n\n\
         Start a Debug Adapter Protocol server for a Hermit GDB remote target.\n\
         The DAP attach request must include the guest executable as 'program'\n\
         and the Hermit GDB server address as 'target'."
    );
}

fn default_gdb() -> OsString {
    let system_gdb = Path::new("/usr/bin/gdb");
    if system_gdb.is_file() {
        system_gdb.as_os_str().to_owned()
    } else {
        OsString::from("gdb")
    }
}

fn parse_gdb() -> Result<Option<OsString>, String> {
    let mut gdb = None;
    let mut args = env::args_os().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--gdb" {
            let path = args
                .next()
                .ok_or_else(|| "--gdb requires a path".to_owned())?;
            if gdb.replace(path).is_some() {
                return Err("--gdb may only be specified once".to_owned());
            }
        } else if argument == "-h" || argument == "--help" {
            usage();
            return Ok(None);
        } else {
            return Err(format!(
                "unrecognized argument: {}",
                argument.to_string_lossy()
            ));
        }
    }
    Ok(Some(gdb.unwrap_or_else(default_gdb)))
}

fn main() -> ExitCode {
    let gdb = match parse_gdb() {
        Ok(Some(gdb)) => gdb,
        Ok(None) => return ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hermit-dap: {error}");
            eprintln!("Try 'hermit-dap --help' for more information.");
            return ExitCode::FAILURE;
        }
    };

    let error = Command::new(&gdb)
        .arg("--quiet")
        .arg("--nx")
        .arg("--init-eval-command=set debuginfod enabled off")
        .arg("--init-eval-command=set sysroot /")
        .arg("--interpreter=dap")
        .exec();

    eprintln!(
        "hermit-dap: failed to execute {}: {error}",
        Path::new(&gdb).display()
    );
    ExitCode::FAILURE
}
