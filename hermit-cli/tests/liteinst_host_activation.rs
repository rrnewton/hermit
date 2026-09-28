/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/liteinst.rs"]
mod liteinst_runtime;

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use hermit::liteinst::CaptureDestination;
use hermit::liteinst::CaptureLimits;
use hermit::liteinst::CaptureOptions;
use hermit::liteinst::CaptureTimeouts;
use hermit::liteinst::DestinationProgress;
use hermit::liteinst_bootstrap::EffectiveFilter;
use tracing::metadata::LevelFilter;

#[derive(Default)]
struct LogDestination {
    bytes: Arc<Mutex<Vec<u8>>>,
    progress: DestinationProgress,
}

impl Write for LogDestination {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().unwrap().extend_from_slice(bytes);
        self.progress.acknowledged_data_bytes += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CaptureDestination for LogDestination {
    fn progress(&self) -> DestinationProgress {
        self.progress
    }
}

fn session_guest() -> (tempfile::TempDir, PathBuf) {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let directory = tempfile::tempdir().expect("create activation guest directory");
    let guest = directory.path().join("liteinst_session");
    let output = std::process::Command::new("cc")
        .args(["-O0", "-fno-pie", "-no-pie", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/liteinst_host_activation.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("compile activation guest");
    assert!(output.status.success(), "{output:?}");
    (directory, guest)
}

#[test]
fn exact_staged_runtime_runs_through_caller_owned_session() {
    liteinst_runtime::ensure_liteinst_runtime();
    let runtime = liteinst_runtime::liteinst_runtime_library();
    let (_directory, guest) = session_guest();
    let destination = LogDestination::default();
    let records = destination.bytes.clone();
    let options = CaptureOptions {
        limits: CaptureLimits {
            producers: 64,
            slots_per_producer: 16,
            max_record_bytes: 1024 * 1024,
            host_pending_bytes: 8 * 1024 * 1024,
            guest_pending_bytes: 8 * 1024 * 1024,
            pending_records: 1024,
            diagnostic_bytes: 64 * 1024,
        },
        timeouts: CaptureTimeouts {
            startup: Duration::from_secs(10),
            blocked_publication: Duration::from_secs(10),
            final_drain: Duration::from_secs(2),
        },
    };
    let filter = EffectiveFilter::from_directives_lossy("detcore=info", LevelFilter::INFO);
    let (mut session, input, host) = hermit::liteinst::prepare(options, destination, &filter)
        .expect("prepare caller-owned LiteInst capture session");
    session.retain_record_status(|| None).unwrap();
    let pending = hermit::run_with_output_backend_timeout_and_log(
        hermit::Command::new(&guest),
        hermit::DetConfig::default(),
        false,
        &None,
        Some(Duration::from_secs(10)),
        input.with_runtime_path(runtime),
    );
    drop(host);
    let finalized = session.finish(pending, Instant::now() + Duration::from_secs(3));
    assert!(
        finalized.evidence.capture.qualifies(),
        "LiteInst capture did not qualify: {:?}",
        finalized.evidence.capture
    );
    let output = finalized
        .result
        .expect("caller-owned LiteInst execution failed");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"getpid-ok\n");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(
        !records.lock().unwrap().is_empty(),
        "LiteInst execution produced no captured records"
    );
}
