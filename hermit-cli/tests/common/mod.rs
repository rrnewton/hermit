/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;

/// Runs a test `body` that is expected to fail under the DBI (DynamoRIO) backend.
///
/// The point of marking a test xfail is to keep *exercising* it, so we notice
/// when the expected failure mode changes:
///
/// - On any backend other than DBI, `body` runs normally and its outcome (pass
///   or panic) propagates unchanged.
/// - On the DBI backend, `body` is run and expected to panic. A panic is
///   reported as an expected failure (`DBI_XFAIL`) and swallowed, so the test
///   passes. If `body` unexpectedly succeeds, that is a `DBI_XPASS`: we panic so
///   the now-stale xfail annotation is noticed and removed.
///
/// This replaces the earlier behavior of returning *before* the test body when
/// running under DBI, which counted xfail tests as passes without ever running
/// them (a false green that hid real failures).
pub fn xfail_dbi<F: FnOnce()>(reason: &str, body: F) {
    xfail(running_under_dbi(), reason, body);
}

/// Whether the active Hermit backend is DBI, per `HERMIT_BACKEND` (the same
/// variable Hermit itself reads to select the backend).
fn running_under_dbi() -> bool {
    std::env::var("HERMIT_BACKEND").as_deref() == Ok("dbi")
}

/// Core of [`xfail_dbi`], factored out so the expected-failure logic can be
/// tested without touching process-wide environment variables.
///
/// When `expect_failure` is false, `body` runs normally and its outcome
/// propagates. When true, `body` is run and *expected* to panic: a panic is
/// swallowed and reported as an expected failure, while an unexpected success
/// panics as an `XPASS` so the stale annotation is noticed.
fn xfail<F: FnOnce()>(expect_failure: bool, reason: &str, body: F) {
    if !expect_failure {
        body();
        return;
    }

    match catch_unwind(AssertUnwindSafe(body)) {
        Err(_) => {
            // Expected: the test body failed under DBI, as annotated. The panic
            // has already been printed by the default hook; the caught unwind is
            // swallowed here so the test is reported as passing.
            eprintln!("DBI_XFAIL (expected failure): {reason}");
        }
        Ok(()) => {
            panic!(
                "DBI_XPASS: test passed under DBI but is marked xfail ({reason}); \
                 the backend now handles this case, so remove the xfail",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::xfail;

    #[test]
    fn non_xfail_runs_body_and_propagates_success() {
        let mut ran = false;
        xfail(false, "reason", || ran = true);
        assert!(ran, "body must run when not xfail");
    }

    #[test]
    #[should_panic(expected = "real failure")]
    fn non_xfail_propagates_failure() {
        xfail(false, "reason", || panic!("real failure"));
    }

    #[test]
    fn xfail_swallows_expected_failure() {
        let mut ran = false;
        // A panicking body is the expected outcome; xfail must run it and not
        // propagate the panic.
        xfail(true, "reason", || {
            ran = true;
            panic!("expected failure under DBI");
        });
        assert!(ran, "body must actually run under xfail");
    }

    #[test]
    #[should_panic(expected = "DBI_XPASS")]
    fn xfail_reports_unexpected_pass() {
        xfail(true, "reason", || { /* unexpectedly succeeds */ });
    }
}
