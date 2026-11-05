//! Runs the `/voice` page's own test suite as part of `cargo test`.
//!
//! The page is plain browser JavaScript with no build step, so its behaviour cannot be asserted
//! from Rust directly — but it is also the surface the owner touches, and it had a real bug
//! (errors visible only in the dev console). A suite that only runs when somebody remembers to run
//! it is a suite that goes stale, so it hangs off the same `cargo test` everything else does.
//!
//! `tests/js/voice_page.test.mjs` executes `web/voice.js` against a small strict DOM; see its
//! header for what that fixture can and cannot fake.
//!
//! # This does not skip
//!
//! If Node is missing, this FAILS. A test that turns itself off when a tool is absent reports
//! green for a suite that never ran, and the page's error handling is exactly the kind of thing
//! that would then rot unnoticed.

use std::path::PathBuf;
use std::process::Command;

const SUITE: &str = "tests/js/voice_page.test.mjs";

#[test]
fn the_voice_page_suite_passes() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new("node")
        .current_dir(&root)
        // Importing `node:test` runs the suite and emits TAP. Calling the file directly works on
        // Node 16 too; its experimental `--test` mode otherwise reports the whole file as one test.
        .arg(SUITE)
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "could not run node, which {SUITE} needs: {error}. Install Node (any recent \
                 version; no packages, no lockfile) and re-run. This test does not skip: a \
                 skipped page suite reads as green while the page is untested."
            )
        });
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Counts, not just the exit code: a suite whose file failed to load exits nonzero, but a
    // suite that silently collected NOTHING would exit zero with `# pass 0`.
    let count = |label: &str| -> Option<u32> {
        text.lines()
            .filter_map(|line| line.trim_start().strip_prefix(&format!("# {label} ")))
            .filter_map(|n| n.trim().parse().ok())
            .max()
    };
    let passed = count("pass").unwrap_or_else(|| panic!("node reported no pass count:\n{text}"));
    let failed = count("fail").unwrap_or_else(|| panic!("node reported no fail count:\n{text}"));

    assert!(
        output.status.success(),
        "the /voice page suite failed:\n{text}"
    );
    assert_eq!(failed, 0, "the /voice page suite failed:\n{text}");
    // Raise this with the suite. It is a floor against a suite that stopped COLLECTING, so it has
    // to track the real count; left behind, it would keep passing while half the file went missing.
    assert!(
        passed >= 294,
        "the /voice page suite ran only {passed} tests, so something stopped collecting them:\n\
         {text}"
    );
}
