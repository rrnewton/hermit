/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Real owned-signal lifetime through consecutive prototype Tool RPCs.
//! This explicitly selected prototype is not Detcore/WaitChild coverage. The
//! separate Hermit CLI regression exercises that integration. Two controlled
//! publishers give two callbacks here because the first is already detached;
//! this is not a general one-callback-per-child rule.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use reverie::GlobalTool;
use reverie::Tid;
use reverie_dbt::DbtRunner;
use reverie_dbt::owned_signal_publication_probe::OwnedSignalPublicationProbeRequest;
use reverie_dbt::sync_rpc::RPC_SOCKET_ENV;
use reverie_rpc_transport::RpcServer;
use reverie_rpc_transport::native_control::ENV;
use reverie_rpc_transport::native_control::NativeControlHub;
use reverie_rpc_transport::native_control::NativeControlOperation;
use reverie_rpc_transport::native_control::NativeControlReply;
use reverie_rpc_transport::native_control::NativePeerIdentity;
use reverie_rpc_transport::native_control::PendingNativeCall;
use reverie_rpc_transport::native_control::PublicationRequest;
use reverie_rpc_transport::native_control::SignalDomainRecord;
use reverie_rpc_transport::native_control::SignalDomainSnapshot;
use serde_json::json;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

const DEADLINE: Duration = Duration::from_secs(30);
const SIGCHLD_BIT: u64 = 1 << (libc::SIGCHLD - 1);

struct Event {
    from: Tid,
    request: OwnedSignalPublicationProbeRequest,
    response: oneshot::Sender<i64>,
}

struct BarrierGlobal {
    events: mpsc::Sender<Event>,
    received: AtomicU32,
    resolved: AtomicU32,
    resolved_changed: Notify,
}

impl BarrierGlobal {
    fn new() -> (Self, mpsc::Receiver<Event>) {
        let (events, receiver) = mpsc::channel(2);
        (
            Self {
                events,
                received: AtomicU32::new(0),
                resolved: AtomicU32::new(0),
                resolved_changed: Notify::new(),
            },
            receiver,
        )
    }

    fn counts(&self, received: u32, resolved: u32) -> Result<(), String> {
        let actual_received = self.received.load(Ordering::Acquire);
        let actual_resolved = self.resolved.load(Ordering::Acquire);
        check(
            actual_received == received && actual_resolved == resolved,
            &format!(
                "RPC counts changed: received {actual_received}/{received}, resolved {actual_resolved}/{resolved}"
            ),
        )
    }

    async fn await_child_replies(&self) -> Result<(), String> {
        loop {
            let changed = self.resolved_changed.notified();
            if self.resolved.load(Ordering::Acquire) >= 2 {
                return self.counts(3, 2);
            }
            changed.await;
        }
    }
}

impl Default for BarrierGlobal {
    fn default() -> Self {
        Self::new().0
    }
}

#[reverie::global_tool]
impl GlobalTool for BarrierGlobal {
    type Request = OwnedSignalPublicationProbeRequest;
    type Response = i64;
    type Config = ();

    async fn receive_rpc(&self, from: Tid, request: Self::Request) -> i64 {
        self.received.fetch_add(1, Ordering::AcqRel);
        let (response, receiver) = oneshot::channel();
        self.events
            .send(Event {
                from,
                request,
                response,
            })
            .await
            .expect("owned-signal controller disappeared");
        let value = receiver.await.expect("controller withheld ordinary reply");
        self.resolved.fetch_add(1, Ordering::AcqRel);
        self.resolved_changed.notify_one();
        println!(
            "owned-publication progress {}",
            json!({"phase":"ordinary-response-consumed","physical_rpc_from":from.as_raw(),"resolved_sample":self.resolved.load(Ordering::Acquire),"value":value})
        );
        value
    }
}

fn check(condition: bool, message: &str) -> Result<(), String> {
    condition.then_some(()).ok_or_else(|| message.to_owned())
}

fn bytes(words: &[u64; 16]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_ne_bytes()).collect()
}

fn child_info(raw: &[u8], child: i32, status: i32) -> Result<(), String> {
    check(
        raw.len() == std::mem::size_of::<libc::siginfo_t>(),
        "incomplete siginfo",
    )?;
    // Copy a complete Linux siginfo value; no alignment assumption about a Vec.
    let info = unsafe { std::ptr::read_unaligned(raw.as_ptr().cast::<libc::siginfo_t>()) };
    check(
        info.si_signo == libc::SIGCHLD
            && info.si_code == libc::CLD_EXITED
            && unsafe { info.si_pid() } == child
            && unsafe { info.si_status() } == status,
        &format!(
            "siginfo identity differs: signo {}/{}, code {}/{}, physical sender {}/{child}, status {}/{status}",
            info.si_signo,
            libc::SIGCHLD,
            info.si_code,
            libc::CLD_EXITED,
            unsafe { info.si_pid() },
            unsafe { info.si_status() }
        ),
    )
}

fn records(snapshot: &SignalDomainSnapshot) -> Result<&[SignalDomainRecord], String> {
    check(
        snapshot.record_count <= 4 && snapshot.reserved == 0,
        "incomplete domain record set",
    )?;
    let values = &snapshot.records[..snapshot.record_count as usize];
    check(
        values.iter().all(|r| r.generation != 0 && r.reserved == 0)
            && values.iter().filter(|r| r.ownership == 1).count() == snapshot.queued as usize
            && values.iter().filter(|r| r.ownership == 2).count() == snapshot.active as usize
            && values.iter().filter(|r| r.ownership == 3).count() == snapshot.returning as usize
            && snapshot.queued + snapshot.active + snapshot.returning == snapshot.record_count,
        "domain ownership counts do not match complete records",
    )?;
    Ok(values)
}

fn owned_record(snapshot: &SignalDomainSnapshot, kind: u32) -> Result<&SignalDomainRecord, String> {
    let mut found = records(snapshot)?.iter().filter(|r| r.ownership == kind);
    let record = found.next().ok_or("expected ownership record missing")?;
    check(
        found.next().is_none(),
        "unexpected extra record in controlled ownership class",
    )?;
    Ok(record)
}

async fn command(
    call: &PendingNativeCall,
    operation: NativeControlOperation,
) -> Result<NativeControlReply, String> {
    println!(
        "owned-publication control-intent {}",
        json!({"call":call.identity,"operation":operation})
    );
    let outcome = call.command(operation).await;
    println!(
        "owned-publication control-return {}",
        json!({"call":call.identity,"operation":operation,"ok":outcome.is_ok(),"error":outcome.as_ref().err()})
    );
    let reply = outcome?;
    println!(
        "owned-publication control {}",
        json!({"call":call.identity,"operation":operation,"reply":reply})
    );
    check(
        reply.owner == call.identity.owner && reply.observation_error == 0 && reply.reserved == 0,
        "control owner or physical-mask observation changed",
    )?;
    Ok(reply)
}

async fn observe(call: &PendingNativeCall, kind: u32) -> Result<NativeControlReply, String> {
    let reply = command(call, NativeControlOperation::DomainObserve).await?;
    let d = &reply.domain;
    check(
        reply.status == 0
            && d.domain != 0
            && d.version != 0
            && d.owner_pid == call.identity.owner.pid
            && d.owner_tid == call.identity.owner.tid
            && d.owner.image_epoch == call.identity.owner.image_epoch
            && d.owner.callback_epoch == call.identity.owner.callback_epoch
            && d.owner.callback_kind == kind
            && d.owner.reserved == 0
            && if kind == 2 {
                d.owner.admission_token != 0 && d.owner.admission_token == d.admission_token
            } else {
                d.owner.admission_token == 0 && d.admission_token == 0
            },
        "actual SDK domain observation/owner is unavailable or inconsistent",
    )?;
    records(d)?;
    Ok(reply)
}

struct ChildLease {
    fd: Arc<OwnedFd>,
    generation: u64,
}

fn authenticate_peer(call: &PendingNativeCall) -> Result<NativePeerIdentity, String> {
    let peer = call.peer_identity().ok_or("kernel peer identity missing")?;
    println!(
        "owned-publication peer {}",
        json!({"call":call.identity,"peer_pid":peer.pid,"peer_connection":peer.connection})
    );
    check(
        peer.connection == call.identity.connection
            && peer.pid == call.identity.owner.pid
            && peer.pid == call.identity.owner.tid
            && peer.pid > 0
            && call.identity.owner.image_epoch != 0
            && call.identity.owner.callback_epoch != 0,
        "SO_PEERCRED identity differs from the single-thread native callback owner",
    )?;
    check_live_peer(&peer)?;
    Ok(peer)
}

fn check_live_peer(peer: &NativePeerIdentity) -> Result<(), String> {
    let fdinfo = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", peer.pidfd.as_raw_fd()))
        .map_err(|e| e.to_string())?;
    let pid = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))
        .ok_or("SO_PEERPIDFD identity missing")?
        .trim()
        .parse::<i32>()
        .map_err(|e| e.to_string())?;
    println!(
        "owned-publication peer-pidfd {}",
        json!({"observed":pid,"expected":peer.pid})
    );
    check(
        pid == peer.pid,
        "SO_PEERPIDFD identifies a different process",
    )?;
    let mut poll = libc::pollfd {
        fd: peer.pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut poll, 1, 0) };
    check(
        result == 0 && poll.revents == 0,
        &format!(
            "announcing native peer {} is not proven live: poll {result}, revents {}",
            peer.pid, poll.revents
        ),
    )?;
    Ok(())
}

fn lease_child(parent: i32, peer: NativePeerIdentity) -> Result<ChildLease, String> {
    let child = peer.pid;
    // These are authenticated native peer PIDs, not application fork returns.
    // Namespace equality separately permits the fixed parent-native wait to use
    // the physical child number authenticated by the host.
    let own_ns = std::fs::read_link("/proc/self/ns/pid").map_err(|e| e.to_string())?;
    let parent_ns =
        std::fs::read_link(format!("/proc/{parent}/ns/pid")).map_err(|e| e.to_string())?;
    let child_ns =
        std::fs::read_link(format!("/proc/{child}/ns/pid")).map_err(|e| e.to_string())?;
    println!(
        "owned-publication namespaces {}",
        json!({"host":own_ns,"parent":parent_ns,"child":child_ns})
    );
    check(
        own_ns == parent_ns && own_ns == child_ns,
        "prototype parent uses a different PID namespace",
    )?;
    let status =
        std::fs::read_to_string(format!("/proc/{child}/status")).map_err(|e| e.to_string())?;
    for (name, expected) in [("Pid:", child), ("Tgid:", child), ("PPid:", parent)] {
        let value = status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .ok_or("child identity field missing")?
            .trim()
            .parse::<i32>()
            .map_err(|e| e.to_string())?;
        println!(
            "owned-publication identity-field {}",
            json!({"child":child,"field":name,"observed":value,"expected":expected})
        );
        check(value == expected, "child physical identity/parent mismatch")?;
    }
    let stat = std::fs::read_to_string(format!("/proc/{child}/stat")).map_err(|e| e.to_string())?;
    let generation = stat
        .rsplit_once(')')
        .ok_or("invalid child stat")?
        .1
        .split_whitespace()
        .nth(19)
        .ok_or("child start generation missing")?
        .parse::<u64>()
        .map_err(|e| e.to_string())?;
    check(generation != 0, "zero child process generation")?;
    println!(
        "owned-publication generation {}",
        json!({"child":child,"start_ticks":generation})
    );
    // Keep the socket-authenticated pidfd across every /proc read. A second
    // live check rejects death during lookup without reopening a numeric PID.
    check_live_peer(&peer)?;
    Ok(ChildLease {
        fd: peer.pidfd,
        generation,
    })
}

async fn await_exit(child: ChildLease) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let mut poll = libc::pollfd {
            fd: child.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, 30_000) };
        check(
            result == 1 && poll.revents == libc::POLLIN,
            "controlled child did not become pidfd-ready",
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn capture_second(
    pending: &PendingNativeCall,
    before: &NativeControlReply,
    child: i32,
    lease: ChildLease,
    input: &mut UnixStream,
    cancel: bool,
) -> Result<u64, String> {
    let first = *owned_record(&before.domain, 2)?;
    check(
        before.domain.queued == 0 && before.domain.active == 1 && before.domain.returning == 0,
        "first callback does not exclusively own its detached frame",
    )?;
    let reservation = pending.reserve_reply(1)?;
    let request = PublicationRequest {
        owner: before.domain.owner,
        domain: before.domain.domain,
        reservation: 1,
        child_generation: lease.generation,
        child_pid: child,
        reserved: 0,
    };
    let arm = command(pending, NativeControlOperation::PublicationArm { request }).await?;
    check(
        arm.status == 0
            && arm.domain.phase == 1
            && arm.publication.handle.request == request
            && arm.publication.handle.interval != 0
            && arm.current_physical_mask == (before.current_physical_mask | SIGCHLD_BIT),
        "actual active-callback publication Arm was not accepted",
    )?;
    let handle = arm.publication.handle;
    let early = command(pending, NativeControlOperation::PublicationClose { handle }).await?;
    check(
        early.status == 3
            && early.domain.phase == 1
            && early.publication_wait_result == 0
            && early.publication_wait_siginfo_words == [0; 16]
            && early.current_physical_mask == arm.current_physical_mask,
        "premature Close manufactured publication or changed the exclusion lease",
    )?;
    let mut wrong = handle;
    wrong.owner_tid = wrong.owner_tid.checked_add(1).ok_or("owner TID overflow")?;
    let refused = command(
        pending,
        NativeControlOperation::PublicationCancel { handle: wrong },
    )
    .await?;
    check(
        refused.status == 2
            && refused.domain.phase == 1
            && refused.current_physical_mask == arm.current_physical_mask,
        "wrong-owner Cancel mutated the live interval",
    )?;
    input.write_all(b"B").map_err(|e| e.to_string())?;
    // Readiness alone is not publication/capture. The following real-parent
    // positive WNOWAIT and actual linked receipt supply those separate facts.
    await_exit(lease).await?;
    let close = command(pending, NativeControlOperation::PublicationClose { handle }).await?;
    child_info(&bytes(&close.publication_wait_siginfo_words), child, 47)?;
    child_info(&bytes(&close.publication.incoming_siginfo_words), child, 47)?;
    check(
        close.status == 0
            && close.publication_wait_result == 0
            && close.domain.phase == 2
            && close.publication.handle == handle
            && close.publication.raw_entries == 1
            && close.publication.outcome == 1
            && close.publication.victim == 0
            && close.publication.allocation != 0
            && close.publication.allocation != first.generation
            && close.publication.guest_mask == before.domain.guest_mask
            && close.publication.saved_physical_mask == before.current_physical_mask
            && close.current_physical_mask == before.current_physical_mask,
        "Close did not certify the sole second raw event linked separately from active",
    )?;
    let held = observe(pending, 2).await?;
    check(
        held.domain.domain == before.domain.domain
            && held.domain.active == 1
            && held.domain.queued == 1
            && held.domain.returning == 0
            && *owned_record(&held.domain, 2)? == first
            && owned_record(&held.domain, 1)?.generation == close.publication.allocation,
        "second event merged with, replaced, or lost the active first frame",
    )?;
    let terminal = if cancel {
        NativeControlOperation::PublicationCancel { handle }
    } else {
        NativeControlOperation::PublicationRelease {
            receipt: close.publication,
        }
    };
    let released = command(pending, terminal).await?;
    check(
        released.status == 0
            && released.domain.phase == 3
            && released.current_physical_mask == before.current_physical_mask,
        "release/cancel did not preserve exact physical mask and phase3 intent",
    )?;
    let duplicate = command(pending, terminal).await?;
    check(
        duplicate.status == 2
            && duplicate.domain.phase == 3
            && duplicate.current_physical_mask == before.current_physical_mask,
        "consumed interval accepted a duplicate terminal operation",
    )?;
    let after = observe(pending, 2).await?;
    check(
        records(&after.domain)? == records(&held.domain)?
            && after.domain.guest_mask == before.domain.guest_mask,
        "live release/cancel lost or changed an authentic owned record",
    )?;
    reservation.finish()?;
    Ok(close.publication.allocation)
}

async fn empty_serial_interval(
    pending: &PendingNativeCall,
    before: &NativeControlReply,
    child: i32,
    child_generation: u64,
) -> Result<(), String> {
    // Deliberate native negative: B is still an unreaped real child, but no
    // second notification is published. A fresh interval must not inherit the
    // preceding interval's capture. The shared adapter never grants an Exit
    // twice; this only tests the endpoint's literal no-new-record outcome.
    let reservation = pending.reserve_reply(2)?;
    let request = PublicationRequest {
        owner: before.domain.owner,
        domain: before.domain.domain,
        reservation: 2,
        child_generation,
        child_pid: child,
        reserved: 0,
    };
    let arm = command(pending, NativeControlOperation::PublicationArm { request }).await?;
    let handle = arm.publication.handle;
    check(
        arm.status == 0
            && arm.domain.phase == 1
            && handle.interval != before.domain.interval
            && handle.interval != 0,
        "phase3 cannot attach a fresh, independently owned interval",
    )?;
    let close = command(pending, NativeControlOperation::PublicationClose { handle }).await?;
    child_info(&bytes(&close.publication_wait_siginfo_words), child, 47)?;
    check(
        close.status == 3
            && close.domain.phase == 2
            && close.publication_wait_result == 0
            && close.publication.raw_entries == 0
            && close.publication.outcome == 0
            && close.publication.allocation == 0
            && close.publication.victim == 0
            && records(&close.domain)? == records(&before.domain)?,
        "positive repeated wait reused an earlier interval's capture",
    )?;
    let cancel = command(
        pending,
        NativeControlOperation::PublicationCancel { handle },
    )
    .await?;
    check(
        cancel.status == 0
            && cancel.domain.phase == 3
            && cancel.current_physical_mask == before.current_physical_mask
            && records(&cancel.domain)? == records(&before.domain)?,
        "finite NO_RECORD cancellation changed the existing pending domain",
    )?;
    reservation.finish()
}

async fn exercise(
    global: &BarrierGlobal,
    events: &mut mpsc::Receiver<Event>,
    hub: &NativeControlHub,
    input: &mut UnixStream,
    cancel: bool,
) -> Result<Tid, String> {
    println!(
        "owned-publication progress {}",
        json!({"phase":"exercise-start","cancel":cancel})
    );
    // Both children must announce before either is released. Their callbacks
    // and the parent marker can arrive in any order; none is resolved here.
    let mut parent_ready = None;
    let mut children = [None, None];
    for _ in 0..3 {
        let event = events.recv().await.ok_or("identity announcement missing")?;
        let call = hub
            .pending(event.from)
            .ok_or("announcing pending call missing")?;
        println!(
            "owned-publication announcement {}",
            json!({"from":event.from.as_raw(),"request":event.request,"call":call.identity})
        );
        match &event.request {
            OwnedSignalPublicationProbeRequest::Marker {
                ordinal: 1,
                callback_depth: 1,
                completed_signals: 0,
                ..
            } => {
                check(
                    parent_ready.is_none(),
                    "duplicate parent identity announcement",
                )?;
                parent_ready = Some((event, call));
            }
            OwnedSignalPublicationProbeRequest::ChildReady {
                role,
                callback_depth: 1,
                completed_signals: 0,
                ..
            } if (1..=2).contains(role) => {
                let index = (*role - 1) as usize;
                check(
                    children[index].is_none(),
                    "duplicate child role announcement",
                )?;
                children[index] = Some((event, call));
            }
            _ => return Err("unexpected or recursive identity announcement".to_owned()),
        }
    }
    global.counts(3, 0)?;
    let (ready, first_call) = parent_ready.ok_or("parent ready marker missing")?;
    let (virtual_child_a, virtual_child_b, virtual_parent) = match ready.request {
        OwnedSignalPublicationProbeRequest::Marker {
            ordinal: 1,
            child_a,
            child_b,
            virtual_parent,
            callback_depth: 1,
            completed_signals: 0,
        } => (child_a, child_b, virtual_parent),
        _ => return Err("ready marker fields differ".to_owned()),
    };
    let parent_peer = authenticate_peer(&first_call)?;
    let parent = parent_peer.pid;
    // The native callback supplies the physical TID to the RPC envelope. Guest
    // getpid/fork results live in the separate virtual identity payload below.
    check(
        ready.from.as_raw() == parent,
        "parent RPC identity differs from its authenticated physical peer",
    )?;
    let mut mapped = Vec::new();
    let mut child_replies = Vec::new();
    for (index, announcement) in children.into_iter().enumerate() {
        let (event, call) = announcement.ok_or("controlled child announcement missing")?;
        let expected_virtual = if index == 0 {
            virtual_child_a
        } else {
            virtual_child_b
        };
        let child_peer = authenticate_peer(&call)?;
        let child = child_peer.pid;
        println!(
            "owned-publication identity-join {}",
            json!({"role":index+1,"request":event.request,"expected_virtual_child":expected_virtual,"expected_virtual_parent":virtual_parent,"physical_child":child,"physical_parent":parent,"physical_rpc_from":event.from.as_raw(),"call":call.identity})
        );
        check(
            event.request
                == OwnedSignalPublicationProbeRequest::ChildReady {
                    role: (index + 1) as u64,
                    virtual_pid: expected_virtual,
                    virtual_parent,
                    callback_depth: 1,
                    completed_signals: 0,
                }
                && event.from.as_raw() == child
                && event.from != ready.from
                && call.identity.connection != first_call.identity.connection
                && child != parent,
            "child role/virtual fork identity does not join authenticated native peer",
        )?;
        let lease = lease_child(parent, child_peer)?;
        mapped.push((child, lease, event.from, call.identity.connection));
        child_replies.push(event.response);
    }
    check(
        mapped[0].0 != mapped[1].0 && mapped[0].2 != mapped[1].2 && mapped[0].3 != mapped[1].3,
        "two child roles identified the same physical process or RPC connection",
    )?;
    let (child_a, _first_lease, _, _) = mapped.remove(0);
    let (child_b, second_lease, _, _) = mapped.remove(0);
    // ACK permits each child to reach its independent pipe wait, not to exit.
    // B announces before waiting for A's EOF, avoiding a handshake cycle.
    for (index, response) in child_replies.into_iter().enumerate() {
        response
            .send(4_411_392 + index as i64 + 1)
            .map_err(|_| "child announcement receiver disappeared")?;
        println!(
            "owned-publication progress {}",
            json!({"phase":"child-response-sent","role":index+1,"physical_pid":if index==0 {child_a} else {child_b},"value":4_411_392+index as i64+1})
        );
    }
    // Sending an ordinary reply does not mean its Tool future was polled.
    // Wait for both explicit child responses to be consumed before allowing
    // the parent to continue. The enclosing original deadline bounds this wait.
    println!(
        "owned-publication progress {}",
        json!({"phase":"child-response-barrier-await","received":global.received.load(Ordering::Acquire),"resolved":global.resolved.load(Ordering::Acquire)})
    );
    global.await_child_replies().await?;
    println!(
        "owned-publication progress {}",
        json!({"phase":"child-response-barrier-complete","received":global.received.load(Ordering::Acquire),"resolved":global.resolved.load(Ordering::Acquire)})
    );
    let child_generation = second_lease.generation;
    println!(
        "owned-publication progress {}",
        json!({"phase":"child-a-input-write-intent","physical_pid":child_a})
    );
    input.write_all(b"A").map_err(|e| e.to_string())?;
    println!(
        "owned-publication progress {}",
        json!({"phase":"child-a-input-write-complete","physical_pid":child_a})
    );
    ready
        .response
        .send(31_337)
        .map_err(|_| "ready receiver disappeared")?;
    println!(
        "owned-publication progress {}",
        json!({"phase":"parent-response-sent","physical_rpc_from":ready.from.as_raw(),"call":first_call.identity,"value":31_337})
    );
    let mut second_lease = Some(second_lease);
    let mut prior = first_call;
    let mut first_domain = None;
    let mut first_generation = 0;
    let mut second_generation = 0;
    let mut callback_owner = None;
    for ordinal in 1..=2 {
        for checkpoint in 1..=3 {
            println!(
                "owned-publication progress {}",
                json!({"phase":"signal-event-await","expected_ordinal":ordinal,"expected_checkpoint":checkpoint})
            );
            let event = events.recv().await.ok_or("signal checkpoint missing")?;
            println!(
                "owned-publication progress {}",
                json!({"phase":"signal-event-received","physical_rpc_from":event.from.as_raw(),"expected_ordinal":ordinal,"expected_checkpoint":checkpoint,"request":event.request})
            );
            let raw = match &event.request {
                OwnedSignalPublicationProbeRequest::Signal {
                    ordinal: o,
                    checkpoint: c,
                    callback_depth: 1,
                    completed_signals,
                    siginfo,
                } if *o == ordinal && *c == checkpoint && *completed_signals == ordinal - 1 => {
                    siginfo
                }
                _ => return Err("signal callback depth/order/completed prefix changed".to_owned()),
            };
            child_info(
                raw,
                if ordinal == 1 { child_a } else { child_b },
                if ordinal == 1 { 31 } else { 47 },
            )?;
            check(
                event.from == ready.from,
                "callback physical RPC owner changed",
            )?;
            let n = 3 + (ordinal - 1) * 3 + checkpoint;
            global.counts(n as u32, (n - 1) as u32)?;
            let pending = hub
                .pending(event.from)
                .ok_or("signal pending call missing")?;
            check(
                pending.identity.connection == prior.identity.connection
                    && pending.identity.request == prior.identity.request + 1
                    && pending.identity.owner.pid == prior.identity.owner.pid
                    && pending.identity.owner.tid == prior.identity.owner.tid
                    && pending.identity.owner.image_epoch == prior.identity.owner.image_epoch,
                "signal RPC connection/request/physical owner changed",
            )?;
            println!(
                "owned-publication progress {}",
                json!({"phase":"stale-request-check-intent","call":prior.identity})
            );
            check(
                prior.command(NativeControlOperation::DomainObserve).await
                    == Err("native call is no longer pending".to_owned()),
                "completed RPC still accepts control enqueue",
            )?;
            println!(
                "owned-publication progress {}",
                json!({"phase":"stale-request-check-complete","call":prior.identity})
            );
            let observed = observe(&pending, 2).await?;
            let active = owned_record(&observed.domain, 2)?;
            check(
                bytes(&active.siginfo_words) == *raw
                    && observed.domain.active == 1
                    && observed.domain.returning == 0,
                "active authentic record changed",
            )?;
            if checkpoint == 1 {
                check(
                    pending.identity.owner.callback_epoch > prior.identity.owner.callback_epoch,
                    "new signal reused the previous native callback epoch",
                )?;
                callback_owner = Some(observed.domain.owner);
            } else {
                check(
                    Some(observed.domain.owner) == callback_owner,
                    "one borrowed callback changed identity across RPC yields",
                )?;
            }
            if ordinal == 1 && checkpoint == 1 {
                first_domain = Some(observed.domain.domain);
                first_generation = active.generation;
                second_generation = capture_second(
                    &pending,
                    &observed,
                    child_b,
                    second_lease.take().ok_or("child lease consumed twice")?,
                    input,
                    cancel,
                )
                .await?;
            } else if ordinal == 1 {
                check(
                    observed.domain.domain == first_domain.unwrap()
                        && active.generation == first_generation
                        && observed.domain.phase == 3
                        && observed.domain.queued == 1
                        && owned_record(&observed.domain, 1)?.generation == second_generation,
                    "phase3/domain/queued second signal did not survive an RPC yield",
                )?;
                if checkpoint == 2 {
                    empty_serial_interval(&pending, &observed, child_b, child_generation).await?;
                }
            } else {
                check(
                    observed.domain.domain == first_domain.unwrap()
                        && active.generation == second_generation
                        && observed.domain.queued == 0
                        && observed.domain.last_retired == first_generation
                        && observed.domain.retirements == 1,
                    "first return frame was not retired before second owned callback",
                )?;
            }
            event
                .response
                .send(0x535000 + (ordinal * 10 + checkpoint) as i64)
                .map_err(|_| "signal receiver disappeared")?;
            prior = pending;
        }
    }
    let done = events.recv().await.ok_or("completion marker missing")?;
    check(
        done.from == ready.from
            && done.request
                == OwnedSignalPublicationProbeRequest::Marker {
                    ordinal: 2,
                    child_a: virtual_child_a,
                    child_b: virtual_child_b,
                    virtual_parent,
                    callback_depth: 1,
                    completed_signals: 2,
                },
        "guest continued before both real signals completed without recursion",
    )?;
    global.counts(10, 9)?;
    let pending = hub
        .pending(done.from)
        .ok_or("completion pending call missing")?;
    let observed = observe(&pending, 1).await?;
    check(
        observed.domain.domain == first_domain.unwrap()
            && observed.domain.phase == 0
            && observed.domain.record_count == 0
            && observed.domain.retirements == 2
            && observed.domain.last_retired == second_generation,
        "actual SDK domain did not retire both authentic return frames",
    )?;
    done.response
        .send(41_414)
        .map_err(|_| "completion receiver disappeared")?;
    Ok(ready.from)
}

fn compile_guest(directory: &Path) -> std::path::PathBuf {
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/owned_signal_publication");
    let build = directory.join("guest-build");
    let configured = Command::new("cmake")
        .arg("-S")
        .arg(source)
        .arg("-B")
        .arg(&build)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .output()
        .expect("configure fixture");
    assert!(
        configured.status.success(),
        "fixture configure failed: {configured:?}"
    );
    let compiled = Command::new("cmake")
        .arg("--build")
        .arg(&build)
        .args([
            "--target",
            "owned_signal_publication_guest",
            "--parallel",
            "2",
        ])
        .output()
        .expect("compile fixture");
    assert!(
        compiled.status.success(),
        "fixture compile failed: {compiled:?}"
    );
    build.join("owned_signal_publication_guest")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the actual built native client and python3; run explicitly with --ignored"]
async fn authentic_second_exit_survives_borrowed_owned_signal_rpc() {
    let client = std::env::var_os("REVERIE_DBT_CLIENT").expect("actual native client is required");
    let directory = tempfile::tempdir().expect("fixture directory").keep();
    println!("owned-publication retained-fixture {}", directory.display());
    let fixture = compile_guest(&directory);
    let launcher = directory.join("bounded-launcher");
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
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Two declared executions: successful Release, and live Cancel preserving
    // the authentic second record. Neither is a retry or a substitute for RED.
    for cancel in [false, true] {
        let sockets = tempfile::tempdir_in("/tmp").expect("short socket directory");
        let socket = sockets.path().join("owned-publication.sock");
        let (global, mut events) = BarrierGlobal::new();
        let global = Arc::new(global);
        let hub = NativeControlHub::new();
        let mut server = RpcServer::bind(&socket, global.clone(), ())
            .expect("bind server")
            .with_native_control(hub.clone());
        let connections = server.connection_monitor();
        let issues = server.retain_connection_issues();
        let server_task = tokio::spawn(server.serve());
        let mut guest = Command::new(&fixture);
        guest
            .env(RPC_SOCKET_ENV, socket)
            .env(ENV, "1")
            .env("HERMIT_DBT_TEST_OWNED_SIGNAL_PUBLICATION", "1")
            .env(
                "REVERIE_DBT_TEST_PROGRAM",
                reverie_dbt::bundled_drrun_path(),
            );
        let runner = DbtRunner::new(&launcher, &client)
            .expect("real native runner")
            .client_argument("-external-global");
        let (reader, mut input) = UnixStream::pair().expect("independent child release channel");
        let execution =
            tokio::task::spawn_blocking(move || runner.output_with_reader(&guest, reader));
        let control = tokio::time::timeout(
            DEADLINE,
            exercise(&global, &mut events, &hub, &mut input, cancel),
        )
        .await;
        // Close host input even after a control failure, before joining the
        // bounded guest: the launcher's input pump must not await our own writer.
        drop(input);
        drop(events);
        let output = execution
            .await
            .expect("launcher task")
            .expect("actual guest execution");
        let mode = if cancel { "cancel" } else { "release" };
        std::fs::write(directory.join(format!("{mode}.stdout")), &output.stdout).unwrap();
        std::fs::write(directory.join(format!("{mode}.stderr")), &output.stderr).unwrap();
        let drained =
            tokio::time::timeout(Duration::from_secs(2), connections.wait_for_idle()).await;
        let connection_issues = issues.snapshot();
        issues.planned_shutdown();
        server_task.abort();
        let server_result = server_task.await;
        println!(
            "owned-publication result {}",
            json!({"mode":mode,"status":output.status.code(),
            "stdout":String::from_utf8_lossy(&output.stdout),"stderr":String::from_utf8_lossy(&output.stderr)})
        );
        let from = control
            .expect("owned-signal control exceeded 30 seconds")
            .expect("owned-signal publication contract failed");
        assert!(output.status.success(), "actual guest failed: {output:?}");
        assert_eq!(
            output.stdout,
            b"owned signal publication completed first=31 second=47\n"
        );
        assert!(drained.is_ok(), "guest connection did not close");
        assert!(
            connection_issues.is_empty(),
            "RPC issues: {connection_issues:?}"
        );
        assert!(
            matches!(server_result,Err(error) if error.is_cancelled()),
            "server exited before planned shutdown"
        );
        global
            .counts(10, 10)
            .expect("all eight parent and two child RPCs resolved exactly once");
        assert!(
            hub.pending(from).is_none(),
            "pending native call survived guest exit"
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TscCaptureMode {
    NeverArmed,
    Release,
    Cancel,
}

/// A TSC borrow owns no active signal frame. This deliberately does not reuse
/// capture_second's active-frame premise from the unchanged original fixture.
async fn capture_at_tsc(
    pending: &PendingNativeCall,
    before: &NativeControlReply,
    child: i32,
    lease: ChildLease,
    child_response: oneshot::Sender<i64>,
    ordinal: u64,
    mode: TscCaptureMode,
) -> Result<Option<u64>, String> {
    check(
        before.domain.phase == 0
            && before.domain.record_count == 0
            && before.domain.active == 0
            && before.domain.queued == 0
            && before.domain.returning == 0,
        "TSC starts with a complete empty domain, not an active signal frame",
    )?;
    let reservation = if mode == TscCaptureMode::NeverArmed {
        None
    } else {
        Some(pending.reserve_reply(ordinal)?)
    };
    let request = PublicationRequest {
        owner: before.domain.owner,
        domain: before.domain.domain,
        reservation: ordinal,
        child_generation: lease.generation,
        child_pid: child,
        reserved: 0,
    };
    let handle = if reservation.is_some() {
        let arm = command(pending, NativeControlOperation::PublicationArm { request }).await?;
        check(
            arm.status == 0
                && arm.domain.phase == 1
                && arm.publication.handle.request == request
                && arm.publication.handle.interval != 0
                && arm.current_physical_mask == before.current_physical_mask | SIGCHLD_BIT,
            "real generated TSC publication Arm failed",
        )?;
        Some(arm.publication.handle)
    } else {
        None
    };
    // The real child has been held in its own independent Tool RPC since
    // before the parent marker. Releasing it cannot require the parent borrow.
    child_response
        .send(4_411_392 + ordinal as i64)
        .map_err(|_| "held child reply disappeared")?;
    await_exit(lease).await?;
    let Some(handle) = handle else {
        // Do not manufacture a capture receipt from child readiness.
        return Ok(None);
    };
    let close = command(pending, NativeControlOperation::PublicationClose { handle }).await?;
    let status = if ordinal == 1 { 31 } else { 47 };
    child_info(&bytes(&close.publication_wait_siginfo_words), child, status)?;
    child_info(
        &bytes(&close.publication.incoming_siginfo_words),
        child,
        status,
    )?;
    check(
        close.status == 0
            && close.publication_wait_result == 0
            && close.domain.phase == 2
            && close.publication.handle == handle
            && close.publication.raw_entries == 1
            && close.publication.outcome == 1
            && close.publication.victim == 0
            && close.publication.allocation != 0
            && close.publication.guest_mask == before.domain.guest_mask
            && close.publication.saved_physical_mask == before.current_physical_mask
            && close.current_physical_mask == before.current_physical_mask,
        "real TSC Close lacks exact linked raw child receipt/masks",
    )?;
    let held = observe(pending, 4).await?;
    check(
        held.domain.active == 0
            && held.domain.queued == 1
            && held.domain.returning == 0
            && owned_record(&held.domain, 1)?.generation == close.publication.allocation,
        "real TSC capture did not retain its one queued allocation",
    )?;
    let operation = if mode == TscCaptureMode::Cancel {
        NativeControlOperation::PublicationCancel { handle }
    } else {
        NativeControlOperation::PublicationRelease {
            receipt: close.publication,
        }
    };
    let terminal = command(pending, operation).await?;
    check(
        terminal.status == 0
            && terminal.domain.phase == 3
            && terminal.current_physical_mask == before.current_physical_mask
            && records(&terminal.domain)? == records(&held.domain)?,
        "TSC Release/Cancel did not retain literal allocation and phase3",
    )?;
    let duplicate = command(pending, operation).await?;
    check(
        duplicate.status == 2
            && duplicate.domain.phase == 3
            && duplicate.current_physical_mask == before.current_physical_mask,
        "TSC terminal operation was reusable",
    )?;
    reservation
        .ok_or("missing TSC reply reservation")?
        .finish()?;
    Ok(Some(close.publication.allocation))
}

// Transport identity and SDK callback eligibility are distinct authenticated
// records. observe() joins their image/epoch and physical owner before selection.
struct SelectedTscOwner {
    native: reverie_rpc_transport::native_control::NativeOwner,
    publication: reverie_rpc_transport::native_control::PublicationOwner,
}

async fn exercise_tsc(
    global: &BarrierGlobal,
    events: &mut mpsc::Receiver<Event>,
    hub: &NativeControlHub,
    input: &mut UnixStream,
    mode: TscCaptureMode,
    request_kind: reverie::Rdtsc,
) -> Result<(Tid, Vec<SelectedTscOwner>), String> {
    let mut ready = None;
    let mut children = [None, None];
    for _ in 0..3 {
        let event = events
            .recv()
            .await
            .ok_or("TSC identity announcement missing")?;
        let call = hub
            .pending(event.from)
            .ok_or("TSC announcing call missing")?;
        match &event.request {
            OwnedSignalPublicationProbeRequest::Marker {
                ordinal: 1,
                callback_depth: 1,
                completed_signals: 0,
                ..
            } => {
                check(ready.is_none(), "duplicate TSC parent")?;
                ready = Some((event, call));
            }
            OwnedSignalPublicationProbeRequest::ChildReady {
                role,
                callback_depth: 1,
                completed_signals: 0,
                ..
            } if (1..=2).contains(role) => {
                let index = (*role - 1) as usize;
                check(children[index].is_none(), "duplicate TSC publisher")?;
                children[index] = Some((event, call));
            }
            _ => return Err("unexpected TSC announcement".into()),
        }
    }
    global.counts(3, 0)?;
    let (ready, first) = ready.ok_or("TSC parent absent")?;
    let (a, b, virtual_parent) = match ready.request {
        OwnedSignalPublicationProbeRequest::Marker {
            child_a,
            child_b,
            virtual_parent,
            ..
        } => (child_a, child_b, virtual_parent),
        _ => unreachable!(),
    };
    let parent = authenticate_peer(&first)?.pid;
    check(parent == ready.from.as_raw(), "TSC parent envelope differs")?;
    let mut publishers = Vec::new();
    for (index, child) in children.into_iter().enumerate() {
        let (event, call) = child.ok_or("TSC child absent")?;
        let peer = authenticate_peer(&call)?;
        let physical = peer.pid;
        check(
            event.request
                == OwnedSignalPublicationProbeRequest::ChildReady {
                    role: (index + 1) as u64,
                    virtual_pid: if index == 0 { a } else { b },
                    virtual_parent,
                    callback_depth: 1,
                    completed_signals: 0,
                }
                && event.from.as_raw() == physical
                && physical != parent
                && call.identity.connection != first.identity.connection,
            "TSC child physical/virtual identity join differs",
        )?;
        publishers.push((physical, lease_child(parent, peer)?, event.response));
    }
    check(publishers[0].0 != publishers[1].0, "TSC publishers alias")?;
    ready
        .response
        .send(31_337)
        .map_err(|_| "TSC parent reply disappeared")?;
    let mut owners = Vec::new();
    let mut domain = None;
    let mut retired = 0;
    let mut prior = first;
    for (index, (child, lease, response)) in publishers.into_iter().enumerate() {
        let ordinal = index as u64 + 1;
        let event = events.recv().await.ok_or("real TSC RPC missing")?;
        check(
            event.from == ready.from
                && event.request
                    == OwnedSignalPublicationProbeRequest::Tsc {
                        request: request_kind,
                        ordinal,
                        callback_depth: 1,
                        completed_signals: ordinal - 1,
                    },
            "TSC opcode/count/depth/prior-signal prefix differs",
        )?;
        let call = hub.pending(event.from).ok_or("TSC pending owner missing")?;
        check(
            call.identity.connection == prior.identity.connection
                && call.identity.owner.pid == parent
                && call.identity.owner.tid == parent
                && call.identity.owner.image_epoch == prior.identity.owner.image_epoch
                && call.identity.owner.callback_epoch > prior.identity.owner.callback_epoch,
            "TSC lacks fresh authentic native transport owner",
        )?;
        check(
            prior.command(NativeControlOperation::DomainObserve).await
                == Err("native call is no longer pending".to_owned()),
            "expired request accepted a control operation",
        )?;
        let before = observe(&call, 4).await?;
        check(
            before.domain.retirements == ordinal - 1
                && before.domain.last_retired == retired
                && domain.is_none_or(|d| d == before.domain.domain),
            "prior actual owned allocation did not retire before next TSC",
        )?;
        domain = Some(before.domain.domain);
        owners.push(SelectedTscOwner {
            native: call.identity.owner,
            publication: before.domain.owner,
        });
        let allocation =
            capture_at_tsc(&call, &before, child, lease, response, ordinal, mode).await?;
        event
            .response
            .send(0x1234_5678_2345_6789)
            .map_err(|_| "TSC ordinary reply disappeared")?;
        prior = call;
        let mut active_owner = None;
        for checkpoint in 1..=3 {
            let signal = events.recv().await.ok_or("TSC-owned Signal RPC missing")?;
            let raw = match &signal.request {
                OwnedSignalPublicationProbeRequest::Signal {
                    ordinal: o,
                    checkpoint: c,
                    callback_depth: 1,
                    completed_signals,
                    siginfo,
                } if *o == ordinal && *c == checkpoint && *completed_signals == ordinal - 1 => {
                    siginfo
                }
                _ => return Err("TSC-owned Signal checkpoint/depth/order differs".into()),
            };
            check(
                signal.from == ready.from,
                "TSC signal physical owner changed",
            )?;
            child_info(raw, child, if ordinal == 1 { 31 } else { 47 })?;
            let current = hub.pending(signal.from).ok_or("TSC signal call absent")?;
            check(
                current.identity.connection == prior.identity.connection
                    && current.identity.request == prior.identity.request + 1,
                "TSC signal request continuity differs",
            )?;
            let observed = observe(&current, 2).await?;
            let active = owned_record(&observed.domain, 2)?;
            check(
                observed.domain.domain == domain.unwrap()
                    && observed.domain.active == 1
                    && observed.domain.queued == 0
                    && observed.domain.returning == 0
                    && observed.domain.retirements == ordinal - 1
                    && observed.domain.last_retired == retired
                    && bytes(&active.siginfo_words) == *raw
                    && allocation.is_none_or(|a| a == active.generation),
                "actual TSC-owned allocation/raw bytes/retirement differ",
            )?;
            if checkpoint == 1 {
                check(
                    current.identity.owner.callback_epoch > prior.identity.owner.callback_epoch,
                    "TSC signal reused expired TSC epoch",
                )?;
                active_owner = Some(observed.domain.owner);
            } else {
                check(
                    active_owner == Some(observed.domain.owner),
                    "one Signal borrow changed native ownership",
                )?;
            }
            if checkpoint == 3 {
                retired = active.generation;
            }
            signal
                .response
                .send(0x535000 + (ordinal * 10 + checkpoint) as i64)
                .map_err(|_| "TSC Signal response disappeared")?;
            prior = current;
        }
        input
            .write_all(&[b'A' + index as u8])
            .map_err(|e| e.to_string())?;
    }
    let done = events.recv().await.ok_or("TSC completion marker missing")?;
    check(
        done.from == ready.from
            && done.request
                == OwnedSignalPublicationProbeRequest::Marker {
                    ordinal: 2,
                    child_a: a,
                    child_b: b,
                    virtual_parent,
                    callback_depth: 1,
                    completed_signals: 2,
                },
        "TSC guest did not complete the original two-signal oracle",
    )?;
    let call = hub
        .pending(done.from)
        .ok_or("TSC completion owner absent")?;
    let final_domain = observe(&call, 1).await?;
    check(
        Some(final_domain.domain.domain) == domain
            && final_domain.domain.phase == 0
            && final_domain.domain.record_count == 0
            && final_domain.domain.retirements == 2
            && final_domain.domain.last_retired == retired,
        "TSC actual SDK allocation retirement incomplete",
    )?;
    done.response
        .send(41_414)
        .map_err(|_| "TSC completion reply disappeared")?;
    Ok((ready.from, owners))
}

fn validate_native_tsc_markers(
    stderr: &[u8],
    owners: &[SelectedTscOwner],
    opcode: &str,
    level: u32,
    mode: TscCaptureMode,
    geometry: (u64, u64),
) {
    let text = String::from_utf8_lossy(stderr);
    let markers: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|line| {
            line.strip_prefix("native-tsc-state ").map(|value| {
                serde_json::from_str(value).expect("complete native TSC JSON observation")
            })
        })
        .collect();
    assert_eq!(owners.len(), 2);
    // Preserve all raw markers. Loader/application TSCs outside the selected
    // probe can be real observations without belonging to these two RPCs.
    let mut previous_return = None;
    for selected_owner in owners {
        let owner = selected_owner.native;
        let publication = selected_owner.publication;
        assert_eq!(publication.image_epoch, owner.image_epoch);
        assert_eq!(publication.callback_epoch, owner.callback_epoch);
        assert_eq!(publication.callback_kind, 4);
        assert_eq!(publication.admission_token, 0);
        assert_eq!(publication.reserved, 0);
        let selected: Vec<_> = markers
            .iter()
            .enumerate()
            .filter(|(_, marker)| {
                marker["pid"].as_u64() == Some(owner.pid as u64)
                    && marker["tid"].as_u64() == Some(owner.tid as u64)
                    && marker["image_epoch"].as_u64() == Some(owner.image_epoch)
                    && marker["callback_epoch"].as_u64() == Some(owner.callback_epoch)
                    && marker["callback_kind"].as_u64() == Some(publication.callback_kind as u64)
            })
            .collect();
        assert_eq!(
            selected.len(),
            2,
            "one actual entry/return per authenticated selected owner; duplicates refuse"
        );
        let (entry_index, entry) = selected[0];
        let (return_index, ret) = selected[1];
        assert!(
            entry_index < return_index && previous_return.is_none_or(|prior| prior < entry_index),
            "actual selected marker order"
        );
        previous_return = Some(return_index);
        for marker in [entry, ret] {
            assert_eq!(marker["pid"].as_u64(), Some(owner.pid as u64));
            assert_eq!(marker["tid"].as_u64(), Some(owner.tid as u64));
            assert_eq!(marker["image_epoch"].as_u64(), Some(owner.image_epoch));
            assert_eq!(
                marker["callback_epoch"].as_u64(),
                Some(owner.callback_epoch)
            );
            assert_eq!(marker["callback_kind"].as_u64(), Some(4));
            assert_eq!(marker["opcode"].as_str(), Some(opcode));
            assert_eq!(marker["xmm_valid"].as_bool(), Some(true));
            assert_eq!(marker["fp_bytes"].as_u64(), Some(512));
            assert!(marker["simd_count"].as_u64().unwrap() >= 16);
            assert!(marker["simd_saved_count"].as_u64().unwrap() >= 16);
            if level >= 1 {
                assert_eq!(marker["avx_enabled"].as_bool(), Some(true));
            }
            if level >= 2 {
                assert_eq!(marker["avx512_enabled"].as_bool(), Some(true));
                assert_eq!(marker["zmm_valid"].as_bool(), Some(true));
                assert_eq!(marker["simd_count"].as_u64(), Some(32));
                assert_eq!(marker["simd_saved_count"].as_u64(), Some(32));
                assert_eq!(marker["opmask_count"].as_u64(), Some(8));
            }
        }
        assert_eq!(entry["phase"].as_str(), Some("entry"));
        assert_eq!(ret["phase"].as_str(), Some("return"));
        let entry_branches = entry["branch_count"]
            .as_u64()
            .expect("actual native application branch count");
        assert_eq!(
            ret["branch_count"].as_u64(),
            Some(entry_branches),
            "no application CTI retires during the held single-application-thread TSC borrow"
        );
        let pc = entry["pc"].as_u64().expect("actual decoded instruction PC");
        let fallthrough = entry["fallthrough"]
            .as_u64()
            .expect("actual decoded next PC");
        // The fixture is verified ET_EXEC. Join to independent emitted ELF
        // symbols, never a load bias inferred from this native marker.
        assert_eq!((pc, fallthrough), geometry);
        assert_eq!(ret["pc"], entry["pc"]);
        assert_eq!(ret["fallthrough"], entry["fallthrough"]);
        assert_eq!(
            ret["path"].as_str(),
            Some(if mode == TscCaptureMode::NeverArmed {
                "cache"
            } else {
                "dispatcher"
            })
        );
        assert_eq!(
            ret["publication_phase"].as_u64(),
            Some(if mode == TscCaptureMode::NeverArmed {
                0
            } else {
                3
            })
        );
        assert_eq!(ret["rax"].as_u64(), Some(0x2345_6789));
        assert_eq!(ret["rdx"].as_u64(), Some(0x1234_5678));
        assert_eq!(
            ret["rcx"].as_u64(),
            Some(if opcode == "tscp" {
                0xd15e_a5e1
            } else {
                0x1122_3344_5566_7702
            })
        );
    }
    println!(
        "native-tsc-state observer-coverage {}",
        json!({"raw_markers": markers.len(), "selected_markers": owners.len()*2, "unrelated_markers": markers.len()-owners.len()*2})
    );
}

struct TscGuest {
    path: std::path::PathBuf,
    geometry: [[(u64, u64); 4]; 2],
}

fn compile_tsc_guest(directory: &Path) -> TscGuest {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_tsc_state");
    let build = directory.join("tsc-guest-build");
    let configured = Command::new("cmake")
        .arg("-S")
        .arg(source)
        .arg("-B")
        .arg(&build)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .output()
        .expect("configure TSC guest");
    std::fs::write(directory.join("guest-configure.stdout"), &configured.stdout).unwrap();
    std::fs::write(directory.join("guest-configure.stderr"), &configured.stderr).unwrap();
    assert!(
        configured.status.success(),
        "TSC configure failed: {configured:?}"
    );
    let compiled = Command::new("cmake")
        .arg("--build")
        .arg(&build)
        .args(["--target", "native_tsc_state_guest", "--parallel", "2"])
        .output()
        .expect("compile TSC guest");
    std::fs::write(directory.join("guest-compile.stdout"), &compiled.stdout).unwrap();
    std::fs::write(directory.join("guest-compile.stderr"), &compiled.stderr).unwrap();
    assert!(
        compiled.status.success(),
        "TSC compile failed: {compiled:?}"
    );
    let guest = build.join("native_tsc_state_guest");
    let header = Command::new("readelf")
        .arg("-h")
        .arg(&guest)
        .output()
        .expect("read actual TSC ELF type");
    std::fs::write(directory.join("guest.elf-header"), &header.stdout).unwrap();
    assert!(header.status.success(), "TSC ELF header failed: {header:?}");
    let header_text = String::from_utf8(header.stdout).unwrap();
    let elf_type = header_text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Type:"))
        .expect("actual ELF type");
    assert_eq!(
        elf_type.split_whitespace().next(),
        Some("EXEC"),
        "test guest must have fixed independent symbol addresses"
    );
    let disassembly = Command::new("objdump")
        .args(["-d", "-w"])
        .arg(&guest)
        .output()
        .expect("decode actual TSC guest");
    std::fs::write(directory.join("guest.disassembly"), &disassembly.stdout).unwrap();
    assert!(
        disassembly.status.success(),
        "TSC disassembly failed: {disassembly:?}"
    );
    let decoded = String::from_utf8(disassembly.stdout).unwrap();
    let mut geometry = [[(0, 0); 4]; 2];
    let symbol = |label: &str| {
        let row = decoded
            .lines()
            .find(|line| line.ends_with(label))
            .expect("unique actual ELF symbol");
        u64::from_str_radix(row.split_whitespace().next().unwrap(), 16).expect("symbol address")
    };
    for (op_index, opcode) in ["tsc", "tscp"].into_iter().enumerate() {
        for (width_index, width) in ["xmm", "ymm", "zmm16", "zmm64"].into_iter().enumerate() {
            let label = format!("<{opcode}_{width}_instruction>:");
            let (_, tail) = decoded
                .split_once(&label)
                .expect("actual instruction symbol");
            let instruction = tail.lines().find(|line| !line.trim().is_empty()).unwrap();
            let (address, body) = instruction.split_once(':').expect("actual decoded address");
            let instruction_address =
                u64::from_str_radix(address.trim(), 16).expect("instruction address");
            assert_eq!(instruction_address, symbol(&label));
            let fields: Vec<_> = body.split_whitespace().collect();
            let expected: &[&str] = if opcode == "tsc" {
                &["66", "0f", "31"]
            } else {
                &["66", "0f", "01", "f9"]
            };
            assert_eq!(&fields[..expected.len()], expected, "exact prefixed opcode");
            let fallthrough = symbol(&format!("<{opcode}_{width}_fallthrough>:"));
            assert_eq!(
                fallthrough.checked_sub(instruction_address),
                Some(expected.len() as u64),
                "actual emitted symbols must be adjacent across exactly one decoded instruction"
            );
            assert_eq!(
                fields.last().copied(),
                Some(if opcode == "tsc" { "rdtsc" } else { "rdtscp" }),
                "actual decoded opcode"
            );
            geometry[op_index][width_index] = (instruction_address, fallthrough);
        }
    }
    TscGuest {
        path: guest,
        geometry,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires actual ABI10 native prototype, generated TSC SDK, and native state observer; run explicitly with --ignored"]
async fn generated_tsc_capture_preserves_architectural_state_and_returns_once() {
    let client = std::env::var_os("REVERIE_DBT_CLIENT").expect("actual ABI10 prototype client");
    let directory = tempfile::tempdir().expect("TSC fixture directory").keep();
    println!("native-tsc-state retained-fixture {}", directory.display());
    let fixture = compile_tsc_guest(&directory);
    let launcher = directory.join("bounded-tsc-launcher");
    std::fs::write(&launcher,r#"#!/usr/bin/env python3
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
"#).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Six declared cases, each with separate x87 and MMX state images and two
    // real publishers. They are not retries; the first failed case stops.
    for (opcode, request) in [("tsc", reverie::Rdtsc::Tsc), ("tscp", reverie::Rdtsc::Tscp)] {
        for mode in [
            TscCaptureMode::NeverArmed,
            TscCaptureMode::Release,
            TscCaptureMode::Cancel,
        ] {
            let sockets = tempfile::tempdir_in("/tmp").unwrap();
            let socket = sockets.path().join("tsc.sock");
            let (global, mut events) = BarrierGlobal::new();
            let global = Arc::new(global);
            let hub = NativeControlHub::new();
            let mut server = RpcServer::bind(&socket, global.clone(), ())
                .unwrap()
                .with_native_control(hub.clone());
            let connections = server.connection_monitor();
            let issues = server.retain_connection_issues();
            let server_task = tokio::spawn(server.serve());
            let mut guest = Command::new(&fixture.path);
            guest
                .arg(opcode)
                .env(RPC_SOCKET_ENV, socket)
                .env(ENV, "1")
                .env("HERMIT_DBT_TEST_OWNED_SIGNAL_PUBLICATION", "1")
                .env("HERMIT_DBT_TEST_OWNED_TSC_CAPTURE", "1")
                .env("HERMIT_DBT_TEST_TSC_STATE_OBSERVER", "1")
                .env(
                    "REVERIE_DBT_TEST_PROGRAM",
                    reverie_dbt::bundled_drrun_path(),
                );
            let runner = DbtRunner::new(&launcher, &client)
                .unwrap()
                .client_argument("-external-global");
            let (reader, mut input) = UnixStream::pair().unwrap();
            let execution =
                tokio::task::spawn_blocking(move || runner.output_with_reader(&guest, reader));
            let control = tokio::time::timeout(
                DEADLINE,
                exercise_tsc(&global, &mut events, &hub, &mut input, mode, request),
            )
            .await;
            drop(input);
            drop(events);
            let output = execution
                .await
                .expect("TSC launcher task")
                .expect("TSC native launch");
            let case = format!("{opcode}-{mode:?}");
            std::fs::write(directory.join(format!("{case}.stdout")), &output.stdout).unwrap();
            std::fs::write(directory.join(format!("{case}.stderr")), &output.stderr).unwrap();
            let drained =
                tokio::time::timeout(Duration::from_secs(2), connections.wait_for_idle()).await;
            let connection_issues = issues.snapshot();
            issues.planned_shutdown();
            server_task.abort();
            let server_result = server_task.await;
            println!(
                "native-tsc-state result {}",
                json!({
                    "case":case,"status":output.status.code(),"control":format!("{control:?}"),
                    "stdout":String::from_utf8_lossy(&output.stdout),
                    "stderr":String::from_utf8_lossy(&output.stderr),
                })
            );
            let (from, owners) = control
                .expect("TSC case exceeded unchanged30s")
                .expect("real TSC capture/state contract");
            assert!(
                output.status.success(),
                "actual TSC guest failed: {output:?}"
            );
            let level = (0..=3)
                .find(|level| {
                    output.stdout==format!(
                "native TSC state opcode={opcode} vector_level={level} x87=ok mmx=ok once=2\n"
            ).as_bytes()
                })
                .expect("exact guest state/oracle output");
            validate_native_tsc_markers(
                &output.stderr,
                &owners,
                opcode,
                level,
                mode,
                fixture.geometry[usize::from(opcode == "tscp")][level as usize],
            );
            assert!(drained.is_ok(), "TSC connection did not drain");
            assert!(
                connection_issues.is_empty(),
                "TSC RPC issues: {connection_issues:?}"
            );
            assert!(matches!(server_result,Err(error) if error.is_cancelled()));
            global
                .counts(12, 12)
                .expect("exact TSC+signal+identity RPC count");
            assert!(hub.pending(from).is_none(), "TSC native call survived exit");
        }
    }
}
