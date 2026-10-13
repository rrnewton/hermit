/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Native controls must progress while the real Runtime's Tool RPC is borrowed.
//!
//! This is new-contract coverage for the opt-in transport, not evidence that
//! child-exit publication or the fork-tree determinism regression is repaired.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use reverie::GlobalTool;
use reverie::Tid;
use reverie_dbt::DbtRunner;
use reverie_dbt::native_control_probe::NativeControlProbeRequest;
use reverie_dbt::sync_rpc::RPC_SOCKET_ENV;
use reverie_rpc_transport::RpcServer;
use reverie_rpc_transport::native_control::CaptureHandle;
use reverie_rpc_transport::native_control::ENV;
use reverie_rpc_transport::native_control::NativeControlHub;
use reverie_rpc_transport::native_control::NativeControlOperation;
use reverie_rpc_transport::native_control::NativeControlReply;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

const FIRST_REPLY: i64 = 31_337;
const SECOND_REPLY: i64 = 41_414;
const MAX_BEGIN_ATTEMPTS: u32 = 64;
const CONTROL_DEADLINE: Duration = Duration::from_secs(30);

struct Marker {
    from: Tid,
    request: NativeControlProbeRequest,
    response: oneshot::Sender<i64>,
}

struct BarrierGlobal {
    markers: mpsc::Sender<Marker>,
    received: AtomicU32,
    resolved: AtomicU32,
}

impl BarrierGlobal {
    fn new() -> (Self, mpsc::Receiver<Marker>) {
        let (markers, receiver) = mpsc::channel(2);
        (
            Self {
                markers,
                received: AtomicU32::new(0),
                resolved: AtomicU32::new(0),
            },
            receiver,
        )
    }

    fn counts(&self, received: u32, resolved: u32) -> Result<(), String> {
        check(
            self.received.load(Ordering::Acquire) == received
                && self.resolved.load(Ordering::Acquire) == resolved,
            "ordinary RPC resolved or re-entered before its explicit response",
        )
    }
}

impl Default for BarrierGlobal {
    fn default() -> Self {
        // An accidentally used default has no controller and fails its send.
        Self::new().0
    }
}

#[reverie::global_tool]
impl GlobalTool for BarrierGlobal {
    type Request = NativeControlProbeRequest;
    type Response = i64;
    type Config = ();

    async fn receive_rpc(&self, from: Tid, request: Self::Request) -> i64 {
        self.received.fetch_add(1, Ordering::AcqRel);
        let (response, receive) = oneshot::channel();
        self.markers
            .send(Marker {
                from,
                request,
                response,
            })
            .await
            .expect("native control fixture receiver disappeared");
        let value = receive.await.expect("fixture withheld the ordinary reply");
        self.resolved.fetch_add(1, Ordering::AcqRel);
        value
    }
}

fn check(condition: bool, message: &str) -> Result<(), String> {
    condition.then_some(()).ok_or_else(|| message.to_owned())
}

fn marker_matches(marker: &Marker, ordinal: u64, completed: u64) -> Result<(), String> {
    check(
        marker.request
            == (NativeControlProbeRequest::Marker {
                ordinal,
                callback_depth: 1,
                completed_markers: completed,
            }),
        "marker did not identify one live callback and the exact completed prefix",
    )
}

fn reply_observed(reply: &NativeControlReply) -> Result<(), String> {
    check(
        reply.observation_error == 0 && reply.reserved == 0,
        "native reply lacks an intact physical-mask observation",
    )
}

async fn exercise_controls(
    global: &BarrierGlobal,
    markers: &mut mpsc::Receiver<Marker>,
    hub: &NativeControlHub,
) -> Result<Tid, String> {
    let first = markers.recv().await.ok_or("first marker missing")?;
    marker_matches(&first, 1, 0)?;
    global.counts(1, 0)?;
    // The server publishes this entry before polling receive_rpc. No PID guess
    // or timing loop is needed to find the exact pending request.
    let pending = hub
        .pending(first.from)
        .ok_or("pending native call missing")?;
    let call = pending.identity;
    check(
        call.connection != 0
            && call.request == 1
            && call.owner.pid > 0
            && call.owner.tid > 0
            && call.owner.image_epoch != 0
            && call.owner.callback_epoch != 0,
        "pending call lacks authenticated native owner/generations",
    )?;
    println!("runtime-control first-call {}", json!(call));

    // The real initializer's client thread can still be completing teardown.
    // Refusals do not count as coverage: preserve every literal reply, require
    // nonmutation, and fail if the unchanged eligibility predicate never holds.
    // The count bounds protocol work, not the helper's lifetime; actual OK is
    // the eligibility condition. Do not raise it after exhaustion to get green.
    // There is no retry after a lease is acquired and no sleep-based premise.
    let mut original_mask = None;
    let mut armed = None;
    for attempt in 1..=MAX_BEGIN_ATTEMPTS {
        let reply = pending
            .command(NativeControlOperation::Begin { owner_epoch: 1 })
            .await?;
        println!(
            "runtime-control begin {}",
            json!({ "attempt": attempt, "reply": reply })
        );
        reply_observed(&reply)?;
        check(reply.owner == call.owner, "Begin owner changed")?;
        if let Some(mask) = original_mask {
            check(
                reply.saved_physical_mask == mask,
                "Begin original mask changed",
            )?;
        } else {
            original_mask = Some(reply.saved_physical_mask);
        }
        global.counts(1, 0)?;
        match reply.status {
            0 => {
                let sigchld_bit = 1_u64 << (libc::SIGCHLD - 1);
                check(
                    reply.phase == 1
                        && reply.handle.owner_epoch == 1
                        && reply.handle.token != 0
                        && reply.handle.owner_pid == call.owner.pid
                        && reply.handle.owner_tid == call.owner.tid
                        && reply.current_physical_mask == (reply.saved_physical_mask | sigchld_bit),
                    "Begin did not acquire exactly the native capture lease",
                )?;
                armed = Some(reply);
                break;
            }
            1 => check(
                reply.phase == 0
                    && reply.handle == CaptureHandle::default()
                    && reply.current_physical_mask == reply.saved_physical_mask,
                "UNAVAILABLE changed capture phase, handle, or physical mask",
            )?,
            _ => return Err(format!("unexpected Begin status: {reply:?}")),
        }
    }
    let begin = armed.ok_or("Begin remained unavailable for all 64 commands")?;
    let cancel = pending
        .command(NativeControlOperation::Cancel {
            handle: begin.handle,
        })
        .await?;
    println!("runtime-control cancel {}", json!(cancel));
    reply_observed(&cancel)?;
    check(
        cancel.status == 0
            && cancel.phase == 3
            && cancel.owner == call.owner
            && cancel.handle == begin.handle
            && cancel.saved_physical_mask == begin.saved_physical_mask
            && cancel.current_physical_mask == begin.saved_physical_mask,
        "Cancel did not acknowledge phase3 with the exact original physical mask",
    )?;
    global.counts(1, 0)?;
    // Phase3 acknowledges intent. Waiting for the dispatcher before this reply
    // would itself deadlock: the native callback still owns the Runtime here.
    first
        .response
        .send(FIRST_REPLY)
        .map_err(|_| "first RPC receiver disappeared")?;

    let second = markers.recv().await.ok_or("second marker missing")?;
    marker_matches(&second, 2, 1)?;
    global.counts(2, 1)?;
    check(second.from == first.from, "logical guest identity changed")?;
    let next = hub
        .pending(second.from)
        .ok_or("second pending call missing")?;
    println!("runtime-control second-call {}", json!(next.identity));
    check(
        next.identity.connection == call.connection
            && next.identity.request == call.request + 1
            && next.identity.owner.pid == call.owner.pid
            && next.identity.owner.tid == call.owner.tid
            && next.identity.owner.image_epoch == call.owner.image_epoch
            && next.identity.owner.callback_epoch > call.owner.callback_epoch,
        "second marker did not enter a later callback on the same connection/image",
    )?;
    // This fails locally against the closed first request. It must never enqueue
    // a command on the second request or create another native lease.
    check(
        pending
            .command(NativeControlOperation::Begin { owner_epoch: 2 })
            .await
            == Err("native call is no longer pending".to_owned()),
        "a completed request did not refuse locally before enqueue",
    )?;
    global.counts(2, 1)?;
    // The bridge's callback mirror alone is not SDK retirement evidence.
    // Acquire a fresh lease on this later callback: actual SDK OK establishes
    // eligibility after the first callback unwound. No signal admission is
    // claimed by either of these no-child control transactions.
    let next_begin = next
        .command(NativeControlOperation::Begin { owner_epoch: 2 })
        .await?;
    println!("runtime-control second-begin {}", json!(next_begin));
    reply_observed(&next_begin)?;
    check(
        next_begin.status == 0
            && next_begin.phase == 1
            && next_begin.owner == next.identity.owner
            && next_begin.handle.owner_epoch == 2
            && next_begin.handle.token > begin.handle.token
            && next_begin.handle.owner_pid == call.owner.pid
            && next_begin.handle.owner_tid == call.owner.tid
            && next_begin.saved_physical_mask == begin.saved_physical_mask
            && next_begin.current_physical_mask
                == (next_begin.saved_physical_mask | (1_u64 << (libc::SIGCHLD - 1))),
        "the later callback could not acquire a fresh SDK lease with the original mask",
    )?;
    global.counts(2, 1)?;
    let next_cancel = next
        .command(NativeControlOperation::Cancel {
            handle: next_begin.handle,
        })
        .await?;
    println!("runtime-control second-cancel {}", json!(next_cancel));
    reply_observed(&next_cancel)?;
    check(
        next_cancel.status == 0
            && next_cancel.phase == 3
            && next_cancel.owner == next.identity.owner
            && next_cancel.handle == next_begin.handle
            && next_cancel.saved_physical_mask == begin.saved_physical_mask
            && next_cancel.current_physical_mask == begin.saved_physical_mask,
        "second Cancel did not acknowledge phase3 and restore the exact mask",
    )?;
    global.counts(2, 1)?;
    second
        .response
        .send(SECOND_REPLY)
        .map_err(|_| "second RPC receiver disappeared")?;
    Ok(first.from)
}

fn compile_guest(directory: &Path) -> std::path::PathBuf {
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runtime_capture_control");
    let build = directory.join("guest-build");
    for mut command in [
        {
            let mut command = Command::new("cmake");
            command
                .arg("-S")
                .arg(source)
                .arg("-B")
                .arg(&build)
                .arg("-DCMAKE_BUILD_TYPE=Release");
            command
        },
        {
            let mut command = Command::new("cmake");
            command.arg("--build").arg(&build).args([
                "--target",
                "runtime_capture_guest",
                "--parallel",
                "2",
            ]);
            command
        },
    ] {
        let output = command.output().expect("run fixture CMake command");
        assert!(output.status.success(), "fixture build failed: {output:?}");
    }
    build.join("runtime_capture_guest")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a built DynamoRIO native client and python3; run explicitly with --ignored"]
async fn runtime_capture_control_progresses_while_tool_rpc_is_borrowed() {
    let client = std::env::var_os("REVERIE_DBT_CLIENT")
        .expect("REVERIE_DBT_CLIENT must point to the actual built native client");
    let directory = tempfile::tempdir().expect("fixture tempdir");
    let fixture = compile_guest(directory.path());
    // Short Unix-domain socket path even when the outer harness has a long TMPDIR.
    let sockets = tempfile::tempdir_in("/tmp").expect("socket tempdir");
    let socket = sockets.path().join("native-control.sock");
    let launcher = directory.path().join("bounded-launcher");
    std::fs::write(&launcher, r#"#!/usr/bin/env python3
import os, signal, subprocess, sys
child = subprocess.Popen([os.environ["REVERIE_DBT_TEST_PROGRAM"], *sys.argv[1:]], start_new_session=True)
try:
    try:
        result = child.wait(timeout=30)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            child.wait(timeout=2)
        except subprocess.TimeoutExpired:
            pass
        result = 124
finally:
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    child.wait()
sys.exit(result)
"#).expect("write bounded launcher");
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))
        .expect("make bounded launcher executable");

    let (global, mut markers) = BarrierGlobal::new();
    let global = Arc::new(global);
    let hub = NativeControlHub::new();
    let mut server = RpcServer::bind(&socket, global.clone(), ())
        .expect("bind native control server")
        .with_native_control(hub.clone());
    let connections = server.connection_monitor();
    let issues = server.retain_connection_issues();
    let server_task = tokio::spawn(server.serve());
    let mut guest = Command::new(fixture);
    guest
        .env(RPC_SOCKET_ENV, socket)
        .env(ENV, "1")
        .env("HERMIT_DBT_TEST_NATIVE_CONTROL", "1")
        .env(
            "REVERIE_DBT_TEST_PROGRAM",
            reverie_dbt::bundled_drrun_path(),
        );
    let runner = DbtRunner::new(launcher, client)
        .expect("create real native runner")
        .client_argument("-external-global");
    let execution = tokio::task::spawn_blocking(move || runner.output(&guest));
    let control = tokio::time::timeout(
        CONTROL_DEADLINE,
        exercise_controls(&global, &mut markers, &hub),
    )
    .await;

    // Always join the independently bounded guest and server before asserting a
    // control failure. Dropping a withheld response also fails the real RPC;
    // it never manufactures a successful ordinary result during cleanup.
    drop(markers);
    let output = execution.await;
    let drained = tokio::time::timeout(Duration::from_secs(2), connections.wait_for_idle()).await;
    let connection_issues = issues.snapshot();
    issues.planned_shutdown();
    server_task.abort();
    let server_result = server_task.await;

    let output = output
        .expect("guest launcher task panicked")
        .expect("run actual DBT guest");
    println!(
        "runtime-control guest status={} stdout={:?} stderr={:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let from = control
        .expect("native controls did not finish within 30 seconds")
        .expect("native control progress contract failed");
    assert!(output.status.success(), "actual guest failed: {output:?}");
    assert_eq!(
        output.stdout,
        b"runtime capture guest completed first=31337 second=41414\n"
    );
    assert!(
        drained.is_ok(),
        "native RPC connection did not close after guest exit"
    );
    assert!(
        connection_issues.is_empty(),
        "native RPC issues: {connection_issues:?}"
    );
    assert!(
        matches!(server_result, Err(error) if error.is_cancelled()),
        "server terminated before the planned listener shutdown"
    );
    global
        .counts(2, 2)
        .expect("both original RPCs must resolve exactly once");
    assert!(
        hub.pending(from).is_none(),
        "pending request survived completion"
    );
}
