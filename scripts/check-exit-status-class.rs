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
    let mut found = Vec::new();
    for file in files {
        let Ok(body) = fs::read_to_string(Path::new(file)) else {
            continue;
        };
        found.extend(scan_text(file, &body));
    }
    found
}

/// The matcher itself, over (path, contents), so it can be exercised on fixtures.
///
/// ⚠️ THE SEAM EXISTS SO THE TESTS CAN SEE NARROWING. This ratchet reports a
/// COUNT, and a count going down is ambiguous: sites were declared, or the
/// matcher stopped seeing them. Those are opposite facts and the number cannot
/// tell them apart. The tests below pin each recognised shape by name, so
/// narrowing the matcher fails a NAMED test instead of quietly reading as
/// progress. Do not inline this back into `scan`.
fn scan_text(file: &str, body: &str) -> Vec<Site> {
    let colliding: BTreeMap<u32, &str> = COLLIDING.iter().copied().collect();
    let mut found = Vec::new();
    {
        if EXEMPT_FILES.iter().any(|(f, _)| *f == file) {
            return found;
        }
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
                    file: file.to_owned(),
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

/// ⚠️ THESE TESTS EXIST BECAUSE THE RATCHET REPORTS A COUNT, AND A FALLING COUNT
/// IS AMBIGUOUS. 17 becoming 12 means either five sites were declared, or the
/// matcher stopped recognising five shapes. Those are opposite facts -- one is
/// progress, the other is the gate quietly measuring less -- and the number alone
/// cannot distinguish them. Every recognised shape is therefore pinned by name
/// here, so NARROWING THE MATCHER FAILS A NAMED TEST rather than reading as work
/// done.
///
/// If you are here because a test failed after you edited the matcher: that is
/// the test working. Decide whether the shape should still be caught, and if it
/// should not, delete the case DELIBERATELY and say why in the commit.
#[cfg(test)]
mod tests {
    use super::*;

    const F: &str = "hermit-cli/tests/example.rs";

    fn count(body: &str) -> usize {
        scan_text(F, body).len()
    }

    // ---- each contested value is caught -------------------------------------

    #[test]
    fn catches_bare_one_the_pre_2558_hermit_code() {
        assert_eq!(count("assert_eq!(output.status.code(), Some(1));"), 1);
    }

    #[test]
    fn catches_bare_125_which_should_be_the_constant() {
        assert_eq!(count("assert_eq!(output.status.code(), Some(125));"), 1);
    }

    #[test]
    fn catches_bare_126_and_127_the_guest_fault_codes() {
        assert_eq!(count("assert_eq!(output.status.code(), Some(126));"), 1);
        assert_eq!(count("assert_eq!(output.status.code(), Some(127));"), 1);
    }

    #[test]
    fn ignores_values_that_cannot_be_confused_with_a_hermit_code() {
        // 0 success, 2 clap, 101 Rust panic, 124 timeout(1), 20 fixture.
        for v in [0, 2, 20, 101, 124] {
            let body = format!("assert_eq!(output.status.code(), Some({v}));");
            assert_eq!(count(&body), 0, "Some({v}) must stay out of scope");
        }
    }

    // ---- the SHAPES the matcher must keep recognising ------------------------

    #[test]
    fn catches_a_multi_line_assert_where_the_value_is_on_its_own_line() {
        let body = "assert_eq!(\n    output.status.code(),\n    Some(1),\n    \"msg\"\n);";
        assert_eq!(count(body), 1);
    }

    #[test]
    fn catches_a_predicate_closure_form() {
        let body = "let exposes = |o: &Output| o.status.code() == Some(1);";
        assert_eq!(count(body), 1);
    }

    #[test]
    fn catches_a_match_arm_form() {
        let body = "match output.status.code() {\n    Some(1) => Outcome::Exposed,\n    _ => Outcome::Other,\n}";
        assert_eq!(count(body), 1);
    }

    #[test]
    fn catches_the_value_up_to_six_lines_below_the_status_mention() {
        let mut body = String::from("let c = output.status.code();\n");
        for _ in 0..5 {
            body.push_str("// filler\n");
        }
        body.push_str("assert_eq!(c, Some(1));");
        assert_eq!(count(&body), 1, "the context window must stay >= 6 lines");
    }

    // ---- the guards that keep it from over-reading ---------------------------

    #[test]
    fn catches_a_site_whose_context_says_status_without_calling_code() {
        // ⚠️ PINS THE SECOND HALF OF `is_exit_status_context`. Every other fixture
        // here happens to contain `.code()`, so without this case the `status`
        // clause is untested and could be deleted with all tests still green --
        // measured, that mutation passed 16/16 before this test existed.
        let body = "let status = child.wait().unwrap();\nassert_eq!(status.into_raw(), Some(1));";
        assert_eq!(
            count(body),
            1,
            "a `status` context with no .code() must count"
        );
    }

    #[test]
    fn ignores_some_one_with_no_exit_status_context() {
        assert_eq!(count("let x: Option<u32> = Some(1);"), 0);
    }

    #[test]
    fn ignores_a_non_numeric_or_unclosed_some() {
        assert_eq!(count("let s = Some(name); // status"), 0);
        assert_eq!(count("let s = Some(1u32); // status"), 0);
    }

    // ---- declarations satisfy the gate ---------------------------------------

    #[test]
    fn every_declaration_form_satisfies_the_site() {
        for decl in [
            "// EXIT-CLASS: guest",
            "// EXIT-CLASS: hermit",
            "let e = ExpectedExit::Guest(1);",
            "let e = GuestExit(1);",
            "let e = HermitInternal;",
        ] {
            let body = format!("{decl}\nassert_eq!(output.status.code(), Some(1));");
            assert_eq!(count(&body), 0, "{decl:?} must satisfy the site");
        }
    }

    #[test]
    fn a_declaration_far_above_the_site_does_not_satisfy_it() {
        let mut body = String::from("// EXIT-CLASS: guest\n");
        for _ in 0..6 {
            body.push_str("// filler\n");
        }
        body.push_str("assert_eq!(output.status.code(), Some(1));");
        assert_eq!(count(&body), 1, "a distant declaration must not carry");
    }

    // ---- the exemption is exactly one file, and it is load-bearing -----------

    #[test]
    fn stress_suite_is_exempt_and_nothing_else_is() {
        let body = "let exposes = |o: &Output| o.status.code() == Some(1);";
        assert_eq!(
            scan_text("hermit-cli/tests/stress_suite.rs", body).len(),
            0,
            "stress_suite.rs is exempt: Some(1) there is the GUEST's signal"
        );
        assert_eq!(
            scan_text("hermit-cli/tests/stress_suite_helpers.rs", body).len(),
            1,
            "the exemption must be the exact path, not a prefix"
        );
    }

    #[test]
    fn the_exemption_records_a_reason() {
        for (file, reason) in EXEMPT_FILES {
            assert!(
                reason.len() > 40,
                "{file} is exempt without a substantive reason"
            );
        }
    }

    // ---- reporting -----------------------------------------------------------

    #[test]
    fn a_site_reports_its_file_line_and_value() {
        let sites = scan_text(F, "// pad\nassert_eq!(output.status.code(), Some(127));");
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].file, F);
        assert_eq!(sites[0].line, 2);
        assert_eq!(sites[0].value, 127);
    }

    #[test]
    fn two_sites_on_one_line_are_both_counted() {
        let body = "assert!(status.code() == Some(1) || status.code() == Some(125));";
        assert_eq!(count(body), 2);
    }
}
