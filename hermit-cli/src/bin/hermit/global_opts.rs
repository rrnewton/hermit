/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::fs::File;
use std::path::PathBuf;

use clap::Arg;
use clap::Command;
use clap::Parser;
use clap::ValueEnum;
use clap::builder::PossibleValue;
use clap::builder::TypedValueParser;
use hermit::Backend;
use tracing::metadata::LevelFilter;

use super::tracing::init_file_tracing;
use super::tracing::init_stderr_tracing;

#[derive(Clone)]
struct BackendValueParser;

impl TypedValueParser for BackendValueParser {
    type Value = Backend;

    fn parse_ref(
        &self,
        command: &Command,
        _argument: Option<&Arg>,
        value: &OsStr,
    ) -> Result<Self::Value, clap::Error> {
        let value = value.to_str().ok_or_else(|| {
            clap::Error::raw(
                clap::error::ErrorKind::InvalidUtf8,
                "backend names must be valid UTF-8",
            )
            .with_cmd(command)
        })?;
        let matches = Backend::value_variants()
            .iter()
            .copied()
            .filter(|backend| backend.as_str().starts_with(value))
            .collect::<Vec<_>>();

        match matches.as_slice() {
            [backend] => Ok(*backend),
            [] => {
                let choices = Backend::value_variants()
                    .iter()
                    .map(|backend| backend.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(clap::Error::raw(
                    clap::error::ErrorKind::InvalidValue,
                    format!("unknown backend prefix `{value}`; expected one of: {choices}"),
                )
                .with_cmd(command))
            }
            matches => {
                let matches = matches
                    .iter()
                    .map(|backend| backend.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(clap::Error::raw(
                    clap::error::ErrorKind::InvalidValue,
                    format!("backend prefix `{value}` is ambiguous; matches: {matches}"),
                )
                .with_cmd(command))
            }
        }
    }

    fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
        Some(Box::new(
            Backend::value_variants()
                .iter()
                .map(|backend| PossibleValue::new(backend.as_str())),
        ))
    }
}

/// Hermit provides a sandbox for deterministic and reproducible execution.
/// Arbitrary programs run inside (guests) become deterministic
/// functions of their inputs. Configuration flags control the initial
/// environment.
///
/// See the "run" and "record" subcommands to run programs within hermit.
/// In both modes, the host file system is visible
/// to the command run inside hermit, and the results will depend on the contents
/// (but not timestamps or inode numbers) of those inputs.
///
/// In run mode, networking is disallowed.  Run mode guarantees that if you
/// run twice with the same input files, you will receive bitwise identical
/// outputs from the computation.
///
/// In record mode, inputs (both files and network traffic) are captured
/// in a content addressible store (CAS).  In this preview version of
/// hermit, the CAS is stored locally in your home directory (~/.hermit).
///
/// Below are options common to all subcommands.
#[derive(Debug, Parser, Clone)]
pub struct GlobalOpts {
    /// The verbosity level of log output.
    #[clap(short, long, value_name = "LEVEL", env = "HERMIT_LOG")]
    pub log: Option<LevelFilter>,

    /// Log to a file instead of the terminal.
    #[clap(long, value_name = "FILE", env = "HERMIT_LOG_FILE")]
    pub log_file: Option<PathBuf>,

    /// Select the process instrumentation backend.
    #[clap(
        short = 'b',
        long,
        value_parser = BackendValueParser,
        value_name = "BACKEND"
    )]
    pub backend: Option<Backend>,
}

impl GlobalOpts {
    /// Initalizes tracing. If using a container, this must be done *inside* of
    /// the container because the tracer may create a new thread.
    #[must_use = "This function returns a guard that should not be immediately dropped"]
    pub fn init_tracing(&self) -> Option<impl Drop + use<>> {
        if let Some(path) = &self.log_file {
            let file_writer = File::create(path).expect("Failed to open log file");
            Some(init_file_tracing(self.log, file_writer))
        } else {
            init_stderr_tracing(self.log);
            None
        }
    }
}
