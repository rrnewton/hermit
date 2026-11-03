/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `hermit rewrite-exclusives`: report, and optionally write, the LSE rewrite
//! of an aarch64 ELF file's exclusive retry loops.
//!
//! The same code `hermit run --rewrite-exclusives` uses, exposed on its own so
//! a binary can be inspected before anyone runs it: which loops the rewriter
//! recognises, which it leaves alone and why.

use std::path::PathBuf;

use clap::Parser;
use hermit::Error;
use hermit::ExitStatus;
use hermit::aarch64_lse::elf;

#[derive(Debug, Parser)]
pub struct RewriteExclusivesOpts {
    /// ELF files to inspect.
    #[clap(value_name = "FILE", required = true)]
    files: Vec<PathBuf>,

    /// Write each rewritten file into this directory, under its own name,
    /// instead of only reporting what the rewrite would do.
    #[clap(long, value_name = "DIR")]
    output_dir: Option<PathBuf>,

    /// List every loop left alone, by address, with the reason. Without it,
    /// only their counts by reason are printed.
    #[clap(long)]
    list_refusals: bool,

    /// What to do with a file, or an executable section, that has no ARM
    /// mapping symbols (`$x`, `$d`), as `hermit run
    /// --rewrite-exclusives-stripped` does: `refuse` (the default) leaves its
    /// loops alone as `no-mapping-symbols`; `heuristic` rewrites it by the
    /// stripped-file heuristic, whose residuals `hermit run --help` lists
    /// and which can corrupt program data where it takes data for code.
    #[clap(
        long,
        value_enum,
        value_name = "refuse|heuristic",
        default_value = "refuse"
    )]
    stripped: elf::StrippedRewrite,
}

impl RewriteExclusivesOpts {
    pub fn main(&self) -> Result<ExitStatus, Error> {
        if let Some(dir) = &self.output_dir {
            let mut names = std::collections::HashSet::new();
            for file in &self.files {
                let name = file
                    .file_name()
                    .ok_or_else(|| Error::msg(format!("{} has no file name", file.display())))?;
                anyhow::ensure!(
                    names.insert(name.to_owned()),
                    "two inputs are named {}, so one would overwrite the other in {}",
                    name.to_string_lossy(),
                    dir.display()
                );
            }
            std::fs::create_dir_all(dir)?;
        }
        let mut incomplete = false;
        for file in &self.files {
            let report = match &self.output_dir {
                Some(dir) => {
                    let name = file.file_name().ok_or_else(|| {
                        Error::msg(format!("{} has no file name", file.display()))
                    })?;
                    elf::rewrite_file(file, dir.join(name), self.stripped)?
                }
                None => elf::rewrite_bytes(&mut std::fs::read(file)?, self.stripped)?,
            };
            let Some(report) = report else {
                println!("{}: not an aarch64 ELF image", file.display());
                incomplete = true;
                continue;
            };
            println!(
                "{}: exclusive_instructions={} rewritten={} left_alone={}",
                file.display(),
                report.exclusive_instructions,
                report.patched_total(),
                report.refused_total(),
            );
            for (idiom, count) in &report.patched {
                println!("  rewritten {idiom}: {count}");
            }
            for (reason, count) in &report.refused {
                println!("  left alone {reason}: {count}");
            }
            for segment in &report.skipped_segments {
                println!("  skipped {segment}");
            }
            if self.list_refusals {
                for (addr, why) in &report.refused_sites {
                    println!("  at {addr:#x}: {why}");
                }
            }
            incomplete |= !report.complete();
        }
        // A nonzero status says a file still holds a loop the rewriter
        // refused, or a section it did not read, so a build check can ask for
        // the property directly instead of parsing the report.
        Ok(ExitStatus::Exited(if incomplete { 1 } else { 0 }))
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    /// `--stripped` takes the run's values, `refuse` by default.
    #[test]
    fn stripped_files_are_refused_unless_the_heuristic_is_asked_for() {
        let default =
            RewriteExclusivesOpts::try_parse_from(["rewrite-exclusives", "a.so"]).unwrap();
        assert_eq!(default.stripped, elf::StrippedRewrite::Refuse);
        let heuristic = RewriteExclusivesOpts::try_parse_from([
            "rewrite-exclusives",
            "--stripped=heuristic",
            "a.so",
        ])
        .unwrap();
        assert_eq!(heuristic.stripped, elf::StrippedRewrite::Heuristic);
        assert!(
            RewriteExclusivesOpts::try_parse_from([
                "rewrite-exclusives",
                "--stripped=guess",
                "a.so"
            ])
            .is_err()
        );
    }
}
