//! Shell rendering shared by the manifest command generator and its native controls.

/// Preserve the producer's exit status only after its fresh `--verify-json`
/// report satisfies the current typed canonical-match requirement.
///
/// The caller owns the outer timeout, environment, and complete command argv.
/// This adds no build or fallback: the maintained sibling reader must already
/// exist, or the caller must select it with `VERIFICATION_REPORT_BIN`.
pub fn hermit_verification_command(mode: &str, seed: Option<i64>, command: &str) -> String {
    let report_path = match mode {
        "verify" | "replay" => "\"$cell/captures/verify.json\"".to_owned(),
        "chaos" => format!("\"$cell/captures/verify-seed-{}.json\"", seed.unwrap_or(0)),
        _ => return command.to_owned(),
    };
    format!(
        "( verification_report_bin=${{VERIFICATION_REPORT_BIN:-$(dirname -- \"$hermit_bin\")/verification-report}}; \
         if [ ! -x \"$verification_report_bin\" ]; then \
         printf 'verification-report reader unavailable: %s; build with cargo build -p hermit --bin verification-report or set VERIFICATION_REPORT_BIN\\n' \"$verification_report_bin\" >&2; exit 2; fi; \
         verify_report={report_path}; rm -f -- \"$verify_report\" || exit; \
         verify_status=0; {command} || verify_status=$?; \
         \"$verification_report_bin\" canonical-match \"$verify_report\" || exit; \
         exit \"$verify_status\" )"
    )
}
