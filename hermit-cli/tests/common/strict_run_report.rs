/* SPDX-License-Identifier: BSD-3-Clause */
//! Require the maintained producer's complete canonical verification contract.

use std::path::Path;
use std::process::Output;

use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::VerificationReport;

pub fn assert_canonical_success(path: &Path, output: &Output) {
    let bytes = std::fs::read(path).unwrap_or_else(|error| {
        panic!(
            "missing verification report {}: {error}; status={} stdout={} stderr={}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    });
    let report = VerificationReport::from_current_json_slice(&bytes)
        .expect("complete current verification report");
    report
        .require_canonical_match()
        .expect("matched nonempty canonical INFO parity");
    report
        .require_exact_output_match()
        .expect("exact two-run output and disposition parity");
    assert_eq!(report.guest_exit_code, Some(0));
    assert!(report.guest_signal.is_none());
    let comparison = report.comparison.as_ref().unwrap();
    assert_eq!(comparison.display_name.as_deref(), Some("BitwiseInfoV1"));
    assert_eq!(comparison.log_scope, Some(ComparedLogScope::Info));
    assert_eq!(comparison.compare_io_buffers, Some(true));
    assert_eq!(comparison.virtualize_time, Some(true));
    assert_eq!(comparison.strip_lines, Some(false));
    assert_eq!(comparison.canonicalize_addresses, Some(true));
    assert_eq!(comparison.full_trace, Some(true));
    assert_eq!(comparison.exact_remainder, Some(true));
    assert_eq!(comparison.ignore_lines, Some(false));
    assert_eq!(comparison.skip_commit, Some(false));
    assert_eq!(comparison.skip_detlog, Some(false));
}
