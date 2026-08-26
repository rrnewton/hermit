#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Refuse a BARE exit-status integer in a test, for the values that changed meaning.
//!
//! ⚠️ WHAT THIS CAN AND CANNOT DECIDE, because the difference is the whole design.
//! It CANNOT decide whether `Some(1)` is correct at a site. That needs to know
//! whether the number came from the GUEST or from HERMIT, and the two are
//! textually identical: `stress_suite.rs:342` and `flock_exclusion.rs:245` are the
//! same expression and opposite in meaning. It CAN decide whether the site says
//! WHICH CHANNEL produced the number. That is the checkable half, and stating the
//! class is the rule this enforces.
//!
//! A wrong declaration is still possible. What changes is that it becomes a
//! visible claim a reviewer can check, instead of the invisible default it is now.
//!
//! ⚠️ WHY A STATIC CHECK AND NOT A TEST. Measured 2026-08-25 at `cec4602d32a8`:
//! 85 of 133 `hermit-cli/tests` targets are run by NO DAG node. No amount of test
//! execution will ever observe a stale assertion in those 85. A source check does
//! not care whether the test runs, which is the only way this class can be gated.
//!
//! ⚠️ WHY ONLY FOUR VALUES. Counted before the key was chosen: 62 exit-status
//! integer literals live in these tests, and 44 of them cannot be confused with a
//! hermit code -- 12x `Some(0)` success, 6x `Some(2)` clap usage (hermit never
//! chooses 2), 17x `Some(124)` the `timeout(1)` convention, 4x `Some(101)` Rust
//! panic, 1x `Some(20)` fixture. Demanding a declaration at those 44 is friction
//! with no risk behind it, and a noisy gate is exempted into uselessness. The
//! colliding set is the values hermit's own `failure_exit_code` can produce, plus
//! the one it USED to produce before hermit#2558:
//!
//!   1    the pre-#2558 hermit failure code -- AMBIGUOUS, and the entire family
//!   125  HERMIT_INTERNAL_FAILURE_EXIT -- should be the constant, not a literal
//!   126  GuestProgramFault: found, not executable
//!   127  GuestProgramFault: not found

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

/// Exit-status values whose meaning is contested. See the module docs for why the
/// other 44 literals are deliberately out of scope.
const COLLIDING: [(u32, &str); 4] = [
    (
        1,
        "pre-#2558 hermit failure code: guest exit OR stale hermit code",
    ),
    (125, "HERMIT_INTERNAL_FAILURE_EXIT: use the constant"),
    (126, "GuestProgramFault: found but not executable"),
    (127, "GuestProgramFault: not found"),
];

/// A site is satisfied when it names the channel the number came from.
const DECLARATIONS: [&str; 4] = [
    "ExpectedExit::",
    "GuestExit(",
    "HermitInternal",
    "EXIT-CLASS:",
];

/// ⚠️ FILE-LEVEL EXEMPTION, AND THE REASON IS LOAD-BEARING.
///
/// `stress_suite.rs` asserts `Some(1)` as THE GUEST'S OWN SIGNAL, not as a hermit
/// code: `Some(1) => GuestOutcome::Exposed` and
/// `let exposes_bug = |output| output.status.code() == Some(1)`. `Some(1)` there
/// means the stress guest REPRODUCED the bug the suite exists to catch. Flipping
/// those sites to a hermit constant would not fail loudly -- the suite would go on
/// passing while silently detecting nothing, forever.
///
/// ⚠️ AND THE EXEMPTION IS AT FILE LEVEL RATHER THAN PER-SITE ON PURPOSE. The
/// in-file alternative is to annotate each site, which means EDITING
/// `stress_suite.rs`. Task `do_not_flip_stress` is a standing notice that a change
/// to that file inside an exit-code head should be REFUSED ON SIGHT. Recording the
/// reason here keeps the rule enforceable without touching the file it protects.
///
/// Established by agent(hermit-020); five sites, lines 277/317/327/342/365.
const EXEMPT_FILES: [(&str, &str); 1] = [(
    "hermit-cli/tests/stress_suite.rs",
    "Some(1) is the GUEST's signal (bug-exposure detector), not a hermit code; \
     see task do_not_flip_stress. Exempt at file level so the file is never edited.",
)];

/// Measured at `cec4602d32a8`: 22 colliding sites, minus the 5 exempt, is 17.
///
/// ⚠️ This is a RATCHET, not a target. It may only go down. Lowering it is the
/// migration: give each site an `EXIT-CLASS:` or a typed `ExpectedExit`, and for a
/// hermit-chosen value use `HERMIT_INTERNAL_FAILURE_EXIT` rather than `125`.
const BASELINE: usize = 17;

struct Site {
    file: String,
    line: usize,
    value: u32,
    text: String,
}

fn tracked_test_files() -> Vec<String> {
    let out = Command::new("git")
        .args([
            "ls-files",
            "hermit-cli/tests/*.rs",
            "detcore/tests/*.rs",
            "tests/*.rs",
        ])
        .output()
        .expect("git ls-files failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// True when `window` looks like an exit-status comparison rather than any other
/// `Some(n)`. Deliberately loose: a false positive costs one declaration, a false
/// negative is a member this gate can never see.
fn is_exit_status_context(window: &str) -> bool {
    window.contains(".code()") || window.contains("status")
}

fn declared(context: &str) -> bool {
    DECLARATIONS.iter().any(|d| context.contains(d))
}

fn scan(files: &[String]) -> Vec<Site> {
    let colliding: BTreeMap<u32, &str> = COLLIDING.iter().copied().collect();
    let mut found = Vec::new();
    for file in files {
        if EXEMPT_FILES.iter().any(|(f, _)| f == file) {
            continue;
        }
        let Ok(body) = fs::read_to_string(Path::new(file)) else {
            continue;
        };
        let lines: Vec<&str> = body.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let mut rest = *line;
            while let Some(at) = rest.find("Some(") {
                let after = &rest[at + 5..];
                let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
                rest = &after[digits.len()..];
                if digits.is_empty() || !rest.starts_with(')') {
                    continue;
                }
                let Ok(value) = digits.parse::<u32>() else {
                    continue;
                };
                if !colliding.contains_key(&value) {
                    continue;
                }
                let lo = i.saturating_sub(6);
                let hi = (i + 7).min(lines.len());
                if !is_exit_status_context(&lines[lo..hi].join("\n")) {
                    continue;
                }
                let clo = i.saturating_sub(3);
                let chi = (i + 2).min(lines.len());
                if declared(&lines[clo..chi].join("\n")) {
                    continue;
                }
                found.push(Site {
                    file: file.clone(),
                    line: i + 1,
                    value,
                    text: line.split_whitespace().collect::<Vec<_>>().join(" "),
                });
            }
        }
    }
    found
}

fn main() {
    rust_script_prelude::init();
    let gate = std::env::args().any(|a| a == "--gate");
    let sites = scan(&tracked_test_files());
    let colliding: BTreeMap<u32, &str> = COLLIDING.iter().copied().collect();

    for (file, reason) in EXEMPT_FILES {
        println!("exempt: {file}\n        {reason}");
    }
    for site in &sites {
        println!(
            "  {}:{} Some({}) -- {}\n      {}",
            site.file,
            site.line,
            site.value,
            colliding[&site.value],
            site.text.chars().take(72).collect::<String>()
        );
    }
    println!(
        "check-exit-status-class: {} undeclared site(s), baseline {}",
        sites.len(),
        BASELINE
    );

    if !gate {
        return;
    }
    if sites.len() > BASELINE {
        eprintln!(
            "REFUSED: {} undeclared exit-status site(s), baseline {}. A new bare exit \
             integer was added.\n  Say which channel produced it: `// EXIT-CLASS: guest` \
             for the guest's own status, or use HERMIT_INTERNAL_FAILURE_EXIT when hermit \
             chose it. This check cannot tell them apart; that is why the site must.",
            sites.len(),
            BASELINE
        );
        std::process::exit(1);
    }
    if sites.len() < BASELINE {
        eprintln!(
            "check-exit-status-class: {} < baseline {} -- lower BASELINE to {} to keep the \
             ratchet tight.",
            sites.len(),
            BASELINE,
            sites.len()
        );
    }
}
