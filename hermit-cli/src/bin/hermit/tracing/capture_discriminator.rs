// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: BSD-3-Clause

//! Transport/formatter fixture, not a Detcore scheduler or SaBre execution.
//! The same original callsites feed the ordinary file subscriber and a bounded
//! FormatEvent adapter. Only successfully formatted records commit. No global
//! tracing forwarder, target rewriting, clock reset or comparator override is used.

use std::fmt;
use std::fs;
use std::io::Write;
use std::io::{self};
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use reverie::GlobalTool;
use reverie::Tid;
use reverie_rpc_transport::BlockingRpcClient;
use reverie_rpc_transport::RpcServer;
use reverie_rpc_transport::guest_log::CaptureDestination;
use reverie_rpc_transport::guest_log::CaptureLimits;
use reverie_rpc_transport::guest_log::CaptureOptions;
use reverie_rpc_transport::guest_log::CaptureReport;
use reverie_rpc_transport::guest_log::CaptureTimeouts;
use reverie_rpc_transport::guest_log::DestinationProgress;
use reverie_rpc_transport::guest_log::HostProducer;
use reverie_rpc_transport::guest_log::PublishError;
use reverie_rpc_transport::guest_log::RunState;
use reverie_rpc_transport::guest_log::SharedBuffer;
use reverie_rpc_transport::guest_log::ordered;
use reverie_rpc_transport::guest_log::{self};
use tracing::Dispatch;
use tracing::Event;
use tracing::Subscriber;
use tracing::metadata::LevelFilter;
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::FormatEvent;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::format::Format;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::registry::LookupSpan;

const TEST: &str = "tracing::capture_discriminator::original_formatter_and_rpc_causal_chain";
const BOUND: Duration = Duration::from_secs(10);
const MAX_RECORD: usize = 32768;

// These implicit module targets and real spans/fields are the original
// callsites. The direct reference calls these functions too.
mod host_events {
    pub fn startup() {
        let span = tracing::info_span!("fixture_host", run = 73);
        let _entered = span.enter();
        tracing::info!(phase = "startup", count = 1, "host startup");
    }
    pub fn rpc() {
        let span = tracing::info_span!("fixture_host", run = 73);
        let _entered = span.enter();
        tracing::info!(request = 73, edge = "response pending", "host RPC handler");
    }
    pub fn cleanup() {
        let span = tracing::info_span!("fixture_host", run = 73);
        let _entered = span.enter();
        tracing::info!(phase = "cleanup", count = 5, "host cleanup");
    }
}
mod guest_events {
    pub fn before() {
        let span = tracing::info_span!("fixture_guest", run = 73);
        let _entered = span.enter();
        tracing::info!(
            answer = 42,
            edge = "request pending",
            "guest before RPC\ncontinuation retained"
        );
    }
    pub fn after() {
        let span = tracing::info_span!("fixture_guest", run = 73);
        let _entered = span.enter();
        detcore::detlog!(event = detcore::detlog::DetLogEvent::SyscallResult {
            finished_syscall_number: 17
        }; "finish syscall #17 fixture response received");
    }
    pub fn broken_format() {
        struct Broken;
        impl std::fmt::Display for Broken {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                Err(std::fmt::Error)
            }
        }
        tracing::info!(broken = %Broken, "format failure must not commit a prefix");
    }
}

#[derive(Default)]
struct GateState {
    entered: bool,
    released: bool,
}
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}
impl Gate {
    fn wait_entered(&self) {
        let deadline = Instant::now() + BOUND;
        let mut state = self.state.lock().unwrap();
        while !state.entered {
            let left = deadline
                .checked_duration_since(Instant::now())
                .expect("destination did not enter");
            state = self.changed.wait_timeout(state, left).unwrap().0;
        }
    }
    fn release(&self) {
        self.state.lock().unwrap().released = true;
        self.changed.notify_all();
    }
}
struct ReleaseOnDrop(Arc<Gate>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[derive(Clone)]
struct Destination {
    bytes: Arc<Mutex<Vec<u8>>>,
    gate: Arc<Gate>,
    progress: DestinationProgress,
    fail: bool,
}
impl Write for Destination {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let deadline = Instant::now() + BOUND;
        let mut state = self.gate.state.lock().unwrap();
        state.entered = true;
        self.gate.changed.notify_all();
        while !state.released {
            let left = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::TimedOut, "fixture destination gate")
                })?;
            state = self.gate.changed.wait_timeout(state, left).unwrap().0;
        }
        drop(state);
        if self.fail {
            return Err(io::Error::other("injected destination failure"));
        }
        self.bytes.lock().unwrap().extend_from_slice(bytes);
        self.progress.acknowledged_data_bytes += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl CaptureDestination for Destination {
    fn progress(&self) -> DestinationProgress {
        self.progress
    }
}
#[derive(Clone, Default)]
struct DirectOutput(Arc<Mutex<Vec<u8>>>);
impl Write for DirectOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct LimitedRecord(String);
impl fmt::Write for LimitedRecord {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if value.len() > MAX_RECORD.saturating_sub(self.0.len()) {
            return Err(fmt::Error);
        }
        self.0.push_str(value);
        Ok(())
    }
}

enum Producer {
    Host(HostProducer),
    Guest {
        buffer: Arc<ordered::Buffer>,
        writer: Mutex<ordered::Writer>,
    },
}
struct CommitSink {
    producer: Producer,
    orders: Mutex<Vec<u64>>,
    errors: Mutex<Vec<String>>,
}
impl CommitSink {
    fn failed(&self, error: impl fmt::Debug) {
        self.errors.lock().unwrap().push(format!("{error:?}"));
        match &self.producer {
            Producer::Host(host) => host.record_failed(),
            Producer::Guest { buffer, .. } => buffer.fail_guest(),
        }
    }
    fn commit(&self, bytes: &[u8]) -> fmt::Result {
        let result = match &self.producer {
            Producer::Host(host) => host.write_record(bytes),
            Producer::Guest { writer, .. } => {
                writer.lock().unwrap().write_record(bytes, bounded_wait())
            }
        };
        match result {
            Ok(commit) => {
                self.orders.lock().unwrap().push(commit.order);
                Ok(())
            }
            Err(error) => {
                self.failed(error);
                Err(fmt::Error)
            }
        }
    }
}
fn bounded_wait() -> impl FnMut(&SharedBuffer, u32) -> Result<(), PublishError> {
    let deadline = Instant::now() + BOUND;
    move |_, _| {
        if Instant::now() >= deadline {
            return Err(PublishError::Full);
        }
        std::thread::yield_now();
        Ok(())
    }
}
struct CommitFormatter(Arc<CommitSink>);
impl<S, N> FormatEvent<S, N> for CommitFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        _: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut record = LimitedRecord(String::new());
        // This is exactly the default ordinary file FormatEvent, including
        // original metadata, default fields, span context and wall prefix.
        // Committing in a MakeWriter Drop would publish a failed prefix.
        if let Err(error) = Format::default().format_event(ctx, Writer::new(&mut record), event) {
            self.0.failed(error);
            return Err(error);
        }
        self.0.commit(record.0.as_bytes())
    }
}
fn capture_dispatch(producer: Producer) -> (Dispatch, Arc<CommitSink>) {
    let sink = Arc::new(CommitSink {
        producer,
        orders: Mutex::new(Vec::new()),
        errors: Mutex::new(Vec::new()),
    });
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(super::env_filter(LevelFilter::INFO))
        .with_ansi(false)
        .event_format(CommitFormatter(sink.clone()))
        .with_writer(io::sink)
        .finish();
    (Dispatch::new(subscriber), sink)
}
fn emit(dispatch: &Dispatch, event: fn()) {
    tracing::dispatcher::with_default(dispatch, event);
}

#[derive(Default)]
struct FixtureGlobal {
    dispatch: Option<Dispatch>,
    sink: Option<Arc<CommitSink>>,
}
#[reverie::global_tool]
impl GlobalTool for FixtureGlobal {
    type Request = u64;
    type Response = (u64, i32, u64);
    type Config = ();
    async fn receive_rpc(&self, from: Tid, request: u64) -> Self::Response {
        assert_eq!(request, 73);
        emit(self.dispatch.as_ref().unwrap(), host_events::rpc);
        // H is synchronously committed before the real response is serialized.
        let order = *self
            .sink
            .as_ref()
            .unwrap()
            .orders
            .lock()
            .unwrap()
            .last()
            .unwrap();
        (request, from.as_raw(), order)
    }
}

fn child(mode: &str, case: &Path) {
    let fd: i32 = std::env::var("HERMIT_CAPTURE_FIXTURE_FD")
        .unwrap()
        .parse()
        .unwrap();
    // The parent transfers its unique prepared endpoint through exec. This
    // cooperating child owns producer 1 exclusively until FINISH/drop.
    let endpoint = unsafe { UnixStream::from_raw_fd(fd) };
    let buffer = unsafe { ordered::Buffer::receive(endpoint.as_raw_fd()) }.unwrap();
    let writer = unsafe { buffer.activate(1, i64::from(std::process::id())) }.unwrap();
    let (dispatch, sink) = capture_dispatch(Producer::Guest {
        buffer,
        writer: Mutex::new(writer),
    });
    emit(&dispatch, guest_events::before);
    assert_eq!(*sink.orders.lock().unwrap(), vec![2]);
    if mode == "wrong-rpc-order" {
        emit(&dispatch, guest_events::after);
    }
    let socket = UnixStream::connect(std::env::var("HERMIT_CAPTURE_FIXTURE_RPC").unwrap()).unwrap();
    socket.set_read_timeout(Some(BOUND)).unwrap();
    socket.set_write_timeout(Some(BOUND)).unwrap();
    // The test body is a real child test thread, distinct from its process
    // leader. Carry its actual Linux sender TID in the RPC envelope.
    let tid = Tid::from_raw(unsafe { libc::gettid() });
    let rpc = BlockingRpcClient::<FixtureGlobal>::from_connected_stream(socket, tid).unwrap();
    let response = rpc.try_send_rpc(73).unwrap();
    assert_eq!(
        response,
        (
            73,
            tid.as_raw(),
            if mode == "wrong-rpc-order" { 4 } else { 3 }
        )
    );
    if mode != "wrong-rpc-order" {
        emit(&dispatch, guest_events::after);
    }
    if mode == "format-failure" {
        emit(&dispatch, guest_events::broken_format);
    }
    let finished = if mode != "missing-finish" && mode != "format-failure" {
        let Producer::Guest { writer, .. } = &sink.producer else {
            unreachable!()
        };
        writer.lock().unwrap().finish(bounded_wait()).unwrap();
        true
    } else {
        false
    };
    let orders = sink.orders.lock().unwrap().clone();
    let errors = sink.errors.lock().unwrap().clone();
    assert_eq!(
        orders,
        if mode == "wrong-rpc-order" {
            vec![2, 3]
        } else {
            vec![2, 4]
        }
    );
    assert_eq!(errors.is_empty(), mode != "format-failure");
    fs::write(case.join("child.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "pid": std::process::id(), "tid": tid.as_raw(), "orders": orders, "errors": errors, "finish_returned": finished,
        "rpc_response": response,
    })).unwrap()).unwrap();
    drop(rpc);
    drop(dispatch);
    drop(sink);
    drop(endpoint);
}

fn options() -> CaptureOptions {
    CaptureOptions {
        limits: CaptureLimits {
            producers: 4,
            slots_per_producer: 1,
            max_record_bytes: MAX_RECORD,
            host_pending_bytes: 65536,
            guest_pending_bytes: 65536,
            pending_records: 8,
            diagnostic_bytes: 4096,
        },
        timeouts: CaptureTimeouts {
            startup: BOUND,
            blocked_publication: BOUND,
            final_drain: Duration::from_secs(2),
        },
    }
}
struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
fn run_case(root: &Path, mode: &str) -> (Vec<u8>, CaptureReport) {
    let case = root.join(mode);
    fs::create_dir(&case).unwrap();
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(Gate::default());
    let output = Destination {
        bytes: bytes.clone(),
        gate: gate.clone(),
        progress: DestinationProgress::default(),
        fail: mode == "destination-failure",
    };
    // Trusted cooperating mapping: one capture collector, one host admission
    // owner and one exclusive guest incarnation; all emitters quiesce before finish.
    let (mut owner, mut log_sink, host) =
        unsafe { guest_log::prepared_capture(options(), output) }.unwrap();
    let _release = ReleaseOnDrop(gate.clone());
    let handle = owner.handle();
    let endpoint = log_sink.take_prepared_endpoint().unwrap().unwrap();
    let held_lifetime = (mode == "held-lifetime").then(|| endpoint.try_clone().unwrap());
    let (dispatch, sink) = capture_dispatch(Producer::Host(host));
    emit(&dispatch, host_events::startup);
    gate.wait_entered();
    assert!(bytes.lock().unwrap().is_empty());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    // Unix path length is limited. /proc refers to the same actual socket inode
    // under this case directory; the held directory FD supplies the short path.
    let directory = fs::File::open(&case).unwrap();
    let rpc_path = format!(
        "/proc/{}/fd/{}/rpc.sock",
        std::process::id(),
        directory.as_raw_fd()
    );
    let (server, issues, connections) = {
        let _entered = runtime.enter();
        let global = Arc::new(FixtureGlobal {
            dispatch: Some(dispatch.clone()),
            sink: Some(sink.clone()),
        });
        let mut server = RpcServer::bind(&rpc_path, global, ()).unwrap();
        let issues = server.retain_connection_issues();
        let connections = server.connection_monitor();
        (server, issues, connections)
    };
    handle.retain_rpc(issues.clone()).unwrap();
    let server_task = runtime.spawn(server.serve());
    let fd = endpoint.as_raw_fd();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
        .env("HERMIT_CAPTURE_FIXTURE_CHILD", mode)
        .env("HERMIT_CAPTURE_FIXTURE_CASE", &case)
        .env("HERMIT_CAPTURE_FIXTURE_FD", fd.to_string())
        .env("HERMIT_CAPTURE_FIXTURE_RPC", &rpc_path)
        .stdout(Stdio::from(
            fs::File::create(case.join("child.stdout")).unwrap(),
        ))
        .stderr(Stdio::from(
            fs::File::create(case.join("child.stderr")).unwrap(),
        ));
    // Only async-signal-safe fcntl runs between fork/exec. No mapped writer is
    // inherited: the child receives/activates it after exec in child().
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut process = KillOnDrop(command.spawn().unwrap());
    drop(endpoint);
    let deadline = Instant::now() + BOUND;
    let status = loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "child deadline; artifacts: {}",
            case.display()
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    handle.root_reaped();
    assert!(
        status.success(),
        "child failed: {status}; artifacts: {}",
        case.display()
    );
    let child_evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(case.join("child.json")).unwrap()).unwrap();
    assert_eq!(
        child_evidence["pid"].as_u64(),
        Some(u64::from(process.0.id()))
    );
    assert_eq!(
        child_evidence["finish_returned"].as_bool(),
        Some(mode != "missing-finish" && mode != "format-failure")
    );
    handle.run_state(RunState::Succeeded);
    runtime.block_on(async {
        tokio::time::timeout(BOUND, connections.wait_for_idle())
            .await
            .unwrap();
    });
    assert!(issues.snapshot().is_empty());
    issues.planned_shutdown();
    server_task.abort();
    runtime.block_on(async {
        assert!(server_task.await.unwrap_err().is_cancelled());
    });

    // Cleanup belongs to the host after actual child reaping and RPC shutdown.
    // A formatting failure closes guest admission, but must leave host cleanup.
    emit(&dispatch, host_events::cleanup);
    let orders = sink.orders.lock().unwrap().clone();
    assert_eq!(
        orders,
        if mode == "wrong-rpc-order" {
            vec![1, 4, 5]
        } else {
            vec![1, 3, 5]
        }
    );
    assert!(sink.errors.lock().unwrap().is_empty());
    let before = handle.capture_snapshot().unwrap();
    assert_eq!(before.publication.progress.acknowledged_data_bytes, 0);
    assert!(bytes.lock().unwrap().is_empty());
    fs::write(
        case.join("before-destination-release.txt"),
        format!("{before:#?}\n"),
    )
    .unwrap();
    fs::write(
        case.join("host-orders.json"),
        serde_json::to_vec(&orders).unwrap(),
    )
    .unwrap();
    drop(dispatch);
    drop(sink);
    gate.release();
    drop(runtime);
    let report = owner.finish_until(Instant::now() + Duration::from_secs(2));
    let captured = bytes.lock().unwrap().clone();
    fs::write(case.join("capture.log"), &captured).unwrap();
    fs::write(case.join("capture-report.txt"), format!("{report:#?}\n")).unwrap();
    fs::write(
        case.join("summary.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "fixture": "transport/formatter", "mode": mode, "qualifies": report.qualifies(),
            "host_orders": orders, "acknowledged_before_release": 0,
            "acknowledged_final": report.publication.progress.acknowledged_data_bytes,
            "captured_bytes": captured.len(), "child_status": status.code(),
            "root_reaped": report.guest.root_reaped, "peer_closed": report.guest.peer_closed,
        }))
        .unwrap(),
    )
    .unwrap();
    drop(held_lifetime);
    // A late lifetime closure cannot retroactively turn the frozen failure green.
    if mode == "held-lifetime" {
        // Stream diagnostics publish only when the collector exits. Observe
        // that exit after releasing the retained endpoint, without changing
        // the original capture deadline or accepting its failed report.
        let observation_deadline = Instant::now() + Duration::from_secs(2);
        while !handle.capture_snapshot().unwrap().collector_finished {
            assert!(
                Instant::now() < observation_deadline,
                "collector did not exit after endpoint closure"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let after = owner.finish_until(Instant::now());
        assert!(!after.qualifies());
        assert!(after.streams[1].finished);
        assert_eq!(after.error, report.error);
        fs::write(
            case.join("after-lifetime-close.txt"),
            format!("{after:#?}\n"),
        )
        .unwrap();
    }
    (captured, report)
}

fn compare(root: &Path, name: &str, direct: &[u8], captured: &[u8], expected_match: bool) {
    use detcore::logdiff::BitwiseInfoV1Diagnostics;
    use detcore::logdiff::ComparisonSideLabels;
    use detcore::logdiff::try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics;
    let mut diagnostics = Vec::new();
    let (summary, left, right) = try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics(
        direct,
        captured,
        ComparisonSideLabels::new("direct", name),
        BitwiseInfoV1Diagnostics {
            no_color: true,
            ..Default::default()
        },
        &mut diagnostics,
    )
    .unwrap();
    fs::write(root.join(format!("{name}.comparison.txt")), &diagnostics).unwrap();
    fs::write(
        root.join(format!("{name}.verdict.txt")),
        format!("{summary:#?}\nparsed_left={left}\nparsed_right={right}\n"),
    )
    .unwrap();
    assert_eq!(
        summary.matched_with_evidence(),
        expected_match,
        "{name}: {summary:?}"
    );
    assert_eq!(
        summary.compared_left, 5,
        "all original INFO records must be compared"
    );
    if expected_match {
        assert_eq!(summary.compared_right, 5);
    } else {
        assert!(
            summary.diff_found,
            "{name}: strict comparator must detect fault"
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RawSuffixIntegrityError {
    InvalidUtf8,
    UnexpectedCount(usize),
    Incomplete,
    DifferentBytes,
}

// This fixture emits exactly one typed event. Compare its complete serialized
// suffix bytes independently of the product comparator's diagnostic metadata.
// Missing, duplicate or incomplete suffixes also fail this integrity oracle.
fn require_raw_suffix_identity(
    reference: &[u8],
    captured: &[u8],
) -> Result<(), RawSuffixIntegrityError> {
    fn suffix(bytes: &[u8]) -> Result<&str, RawSuffixIntegrityError> {
        let text = std::str::from_utf8(bytes).map_err(|_| RawSuffixIntegrityError::InvalidUtf8)?;
        let separator = detcore::detlog::RECORD_SEPARATOR;
        let count = text.matches(separator).count();
        if count != 1 {
            return Err(RawSuffixIntegrityError::UnexpectedCount(count));
        }
        text.split_once(separator)
            .unwrap()
            .1
            .split_once('\n')
            .map(|(suffix, _)| suffix)
            .ok_or(RawSuffixIntegrityError::Incomplete)
    }
    if suffix(reference)? == suffix(captured)? {
        Ok(())
    } else {
        Err(RawSuffixIntegrityError::DifferentBytes)
    }
}

#[test]
fn original_formatter_and_rpc_causal_chain() {
    if let Ok(mode) = std::env::var("HERMIT_CAPTURE_FIXTURE_CHILD") {
        child(
            &mode,
            &PathBuf::from(std::env::var_os("HERMIT_CAPTURE_FIXTURE_CASE").unwrap()),
        );
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let root = std::env::var_os("HERMIT_CAPTURE_FIXTURE_ARTIFACTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| temporary.path().to_owned());
    fs::create_dir_all(&root).unwrap();
    let output = DirectOutput::default();
    let direct_dispatch = Dispatch::new(super::sync_file_subscriber(
        LevelFilter::INFO,
        output.clone(),
    ));
    for event in [
        host_events::startup,
        guest_events::before,
        host_events::rpc,
        guest_events::after,
        host_events::cleanup,
    ] {
        emit(&direct_dispatch, event);
    }
    drop(direct_dispatch);
    let direct = output.0.lock().unwrap().clone();
    fs::write(root.join("direct.log"), &direct).unwrap();
    let (positive, report) = run_case(&root, "positive");
    assert!(report.qualifies(), "{report:#?}");
    assert_eq!(
        report.publication.progress.acknowledged_data_bytes,
        positive.len() as u64
    );
    compare(&root, "positive", &direct, &positive, true);
    // Canonical message equality does not certify serialized diagnostic
    // metadata. Require raw transport integrity with its own fallible oracle.
    require_raw_suffix_identity(&direct, &positive).unwrap();

    // Controls corrupt captured bytes deliberately; the reference and strict
    // comparator are unchanged. Mutations must really touch the original data.
    let text = String::from_utf8(positive.clone()).unwrap();
    for (name, original, replacement) in [
        (
            "target-loss",
            "capture_discriminator::guest_events",
            "capture_discriminator::erased_target",
        ),
        ("field-loss", "answer=42", ""),
        ("span-loss", "fixture_guest{run=73}", "fixture_guest{}"),
    ] {
        assert!(
            text.contains(original),
            "negative {name} did not find its real formatted payload"
        );
        let changed = text.replace(original, replacement);
        fs::write(root.join(format!("{name}.log")), &changed).unwrap();
        compare(&root, name, &direct, changed.as_bytes(), false);
    }
    // The host H event is non-DETLOG INFO. Removing or duplicating that actual
    // full formatted record exercises selection and count sensitivity.
    let line = text
        .lines()
        .find(|line| line.contains("host RPC handler"))
        .unwrap();
    let record = format!("{line}\n");
    for (name, changed) in [
        ("non-detlog-loss", text.replacen(&record, "", 1)),
        (
            "duplication",
            text.replacen(&record, &format!("{record}{record}"), 1),
        ),
    ] {
        fs::write(root.join(format!("{name}.log")), &changed).unwrap();
        compare(&root, name, &direct, changed.as_bytes(), false);
    }
    let (wrong, report) = run_case(&root, "wrong-rpc-order");
    assert!(
        report.qualifies(),
        "ordering control must have a complete capture: {report:#?}"
    );
    compare(&root, "wrong-rpc-order", &direct, &wrong, false);
    for mode in [
        "missing-finish",
        "held-lifetime",
        "format-failure",
        "destination-failure",
    ] {
        let (_, report) = run_case(&root, mode);
        assert!(
            !report.qualifies(),
            "{mode} cannot be a comparator pass: {report:#?}"
        );
        assert!(report.guest.root_reaped);
        match mode {
            "missing-finish" => {
                assert!(report.guest.peer_closed);
                assert!(!report.streams[1].finished);
            }
            "held-lifetime" => {
                assert!(!report.guest.peer_closed);
                assert!(!report.collector_finished);
            }
            "format-failure" => {
                assert!(!report.guest.issues.is_empty());
            }
            "destination-failure" => {
                assert!(report.publication.error.is_some());
            }
            _ => unreachable!(),
        }
        // Intentionally do not invoke the comparator for incomplete captures.
    }

    // Attempt 5's failed expectation is retained in the experiment artifacts.
    // It asked canonical equality to certify suffix bytes it does not compare.
    // Keep the exact 17 -> 18 corruption and require the SAME raw integrity
    // oracle that guards the positive case to reject those changed bytes.
    let original = "\"finished_syscall_number\":17";
    assert!(text.contains(original));
    let changed = text.replace(original, "\"finished_syscall_number\":18");
    fs::write(root.join("typed-counter-corruption.log"), &changed).unwrap();
    let raw_integrity = require_raw_suffix_identity(&direct, changed.as_bytes());
    fs::write(
        root.join("typed-counter-corruption.raw-integrity.txt"),
        format!("{raw_integrity:?}\n"),
    )
    .unwrap();
    assert_eq!(raw_integrity, Err(RawSuffixIntegrityError::DifferentBytes));
    // Record the narrower canonical observation separately. Its acceptance
    // cannot override the raw-integrity rejection above.
    compare(
        &root,
        "typed-counter-corruption-canonical-only",
        &direct,
        changed.as_bytes(),
        true,
    );
}
