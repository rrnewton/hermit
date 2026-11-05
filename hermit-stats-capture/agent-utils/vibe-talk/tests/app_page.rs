//! Runs the phone web app's own test suite as part of `cargo test`.
//!
//! The twin of `tests/voice_page.rs`, and deliberately its twin rather than an extension of it:
//! `/` and `/voice` are two served pages with two suites and two floors, and a runner that pooled
//! them would let one suite stop collecting behind the other's count.
//!
//! It exists because the page at `/` had no suite at all while `/voice` had a large one, and that
//! asymmetry is exactly what let three fixes — `#52 operator-timezone`, `#62
//! message-count-accuracy` and `#39 channel-alias` — land on one page and not the other. A guard
//! over the bytes of both assets lives in `src/http/api.rs`; this is the behavioural half.
//!
//! `tests/js/app_page.test.mjs` executes `web/app.js` against a small strict DOM; see its header
//! for what that fixture can and cannot fake.
//!
//! # This does not skip
//!
//! If Node is missing, this FAILS, for the same reason `tests/voice_page.rs` does: a test that
//! turns itself off when a tool is absent reports green for a suite that never ran.

use std::path::PathBuf;
use std::process::Command;

const SUITE: &str = "tests/js/app_page.test.mjs";

#[test]
fn the_phone_app_suite_passes() {
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
        "the phone app suite failed:\n{text}"
    );
    assert_eq!(failed, 0, "the phone app suite failed:\n{text}");
    // Raise this with the suite. It is a floor against a suite that stopped COLLECTING, so it has
    // to track the real count; left behind, it would keep passing while half the file went missing.
    assert!(
        passed >= 9,
        "the phone app suite ran only {passed} tests, so something stopped collecting them:\n\
         {text}"
    );
}
