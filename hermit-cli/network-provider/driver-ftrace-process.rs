/* SPDX-License-Identifier: BSD-3-Clause */
// Shared stage admission for the package action and ordinary Detcore bridge.
// process_group remains the unchanged supervisor, including its helper tests.
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use serde_json::{Value, json};

use super::process_group::{self, Limits, OwnedChild, bounds, supervise};

const MIB: u64 = 1024 * 1024;
pub(super) const LIMITS: Limits = Limits {
    wall: Duration::from_secs(60),
    logs: 4 * MIB,
    cleanup: Duration::from_secs(2),
};

// Closed import set for the entire actual-driver translation unit, before link/run.
// No libbpf, syscall, filesystem, descriptor or loader import is admitted.
pub(super) const DRIVER_FTRACE_IMPORT_FENCE: &str = r#"
import subprocess, sys
allowed = set('''__assert_fail __errno_location __isoc99_sscanf __isoc23_sscanf
sscanf abort fprintf printf stderr memcmp memcpy memmove memset strcmp strlen strncmp
bcmp __stack_chk_fail _GLOBAL_OFFSET_TABLE_'''.split())
text = subprocess.check_output(['nm', '-u', '--', sys.argv[1]], text=True)
actual = {line.split()[-1] for line in text.splitlines() if line.split()}
print('driver-ftrace undefined imports:', ' '.join(sorted(actual)))
bad = actual - allowed
if bad:
    raise SystemExit('unmocked driver imports: ' + ', '.join(sorted(bad)))
"#;

// Every stage shares the original action start and the same append-only logs.
// A new compiler or test never receives another 60-second or 4-MiB allowance.
pub(super) fn execute_stage(command: &mut Command, started: Instant, stdout: &Path, stderr: &Path) -> Value {
    let admission = (|| -> Result<()> {
        let current = bounds(started, stdout, stderr, LIMITS)?;
        ensure!(
            !current.0 && !current.1,
            "aggregate wall/log bound before next stage"
        );
        process_group::own_descendants()?;
        command
            .stdin(Stdio::null())
            .stdout(File::options().append(true).open(stdout)?)
            .stderr(File::options().append(true).open(stderr)?);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                for (resource, limit) in [
                    (libc::RLIMIT_CORE, 0),
                    (libc::RLIMIT_AS, 4 * 1024 * MIB),
                    (libc::RLIMIT_FSIZE, 64 * MIB),
                ] {
                    let value = libc::rlimit {
                        rlim_cur: limit,
                        rlim_max: limit,
                    };
                    if libc::setrlimit(resource, &value) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(())
    })();
    match admission.and_then(|()| command.spawn().map_err(Into::into)) {
        Ok(child) => supervise(
            OwnedChild {
                child,
                cleanup_attempted: false,
                reaped: false,
            },
            started,
            stdout,
            stderr,
            LIMITS,
        ),
        Err(error) => json!({ "pid":null, "raw_status":null, "signal":null,
            "primary_error":format!("stage admission/spawn: {error:#}"),
            "cleanup_attempted":false, "cleanup_complete":null, "passed":false }),
    }
}
