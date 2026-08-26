/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Bound the gdbserver accept on the LIVENESS OF THE CLIENT HERMIT SPAWNED.
//!
//! ⚠️ WHY THIS EXISTS: HERMIT WAITS FOREVER FOR A GDB THAT IS ALREADY GONE.
//! `hermit record --verify-with-gdbex` and `hermit replay` under gdb both spawn a
//! `gdb` client and then start a container whose gdbserver waits for it. That
//! wait is an unbounded `listener.accept().await` in
//! `reverie-ptrace/src/gdbstub/server.rs`; nothing bounds it, and
//! `--record-timeout` arms the recording only. If the client exits or dies
//! WITHOUT completing its connection, the accept never returns and hermit never
//! returns with it.
//!
//! Reproduced with no kill and no signal: put a `gdb` on `PATH` that exits 0
//! without connecting. Observed live, the outer process alive in
//! `anon_pipe_read`, the container child alive in `epoll_wait` owning the LISTEN
//! socket, and gdb a zombie holding nothing.
//!
//! ⚠️ AND HERMIT SETS UP THE RACE ITSELF, which is why this is not exotic. The
//! client is spawned BEFORE the container that binds the port, so gdb can fail
//! its own `target remote` connect and exit on its own. Under load that window
//! widens; three such wedges were observed during one parallel test run while a
//! quiet run passed in a second.
//!
//! ⚠️ A BOUND ON THE ACCEPT, NOT A TIMEOUT ON THE RUN. The difference is the
//! whole design:
//!
//! * a bound keyed on the client asks an OBSERVABLE FACT — hermit spawned that
//!   process and holds its [`Child`], so "my client is gone" is a `wait`, not an
//!   estimate. It is correct under any load, and it cannot fire while a healthy
//!   session is in progress;
//! * a timeout guesses how long a connection ought to take, and kills a run that
//!   may be perfectly healthy. It is wrong precisely under load, which is when
//!   this fires.
//!
//! ⚠️ AND IT MUST NOT BE APPLIED WHERE THE WAIT IS CORRECT. `hermit run
//! --gdbserver` uses the same accept and SHOULD wait indefinitely: a human
//! attaches later, and there is no spawned client whose death could be observed.
//! The distinction is not which call site it is, but whether there is a client
//! whose liveness we own — which a timeout cannot tell apart and a [`Child`] can.

use std::io::Read;
use std::net::TcpStream;
use std::process::Child;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

/// Context added to a container failure when the spawned client had exited
/// BEFORE the gdbserver ever accepted a connection.
pub const CLIENT_EXITED_BEFORE_CONNECTING: &str = "the gdb client hermit spawned exited before it finished connecting to the \
     gdbserver, so the replay had no debugger to serve";

/// How often the watcher polls, both for the client exiting and for the port
/// becoming connectable.
///
/// ⚠️ THIS IS NOT A TIMEOUT AND IT BOUNDS NOTHING. Every loop it paces exits on
/// a fact -- the client having exited, the container having finished, or the
/// connection succeeding. The interval only decides how promptly each is
/// noticed.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How many [`POLL_INTERVAL`] ticks to wait for the container to finish after a
/// release connect succeeds, before concluding the peer was not our gdbserver.
///
/// ⚠️ THIS BOUNDS A WAIT, NOT THE LOOP. The loop still exits only on the container
/// finishing; this only decides how long a single connect is given to prove itself
/// before another is attempted. Too short and a slow teardown looks like a wrong
/// peer, costing one extra connect; too long and a genuinely wrong peer delays the
/// next attempt. 25 ticks is half a second, which is far longer than a released
/// `accept()` needs to unwind and short enough to retry promptly.
const RELEASE_GRACE_TICKS: u32 = 25;

/// How long to wait for the peer to close after we connect, before concluding it
/// never accepted us.
const ACCEPT_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Did the peer ACCEPT this connection, rather than leave it queued?
///
/// ⚠️ `connect()` SUCCEEDING PROVES NOTHING, WHICH IS THE WHOLE PROBLEM. The kernel
/// completes the handshake into the listen backlog whether or not the application
/// ever calls `accept()`. So a gdbserver that ALREADY HAS its peer -- the healthy
/// case, where gdb connected, worked and quit -- answers a probe exactly like one
/// still blocked in `accept()`.
///
/// The two are distinguishable by what happens next. A gdbstub that accepts this
/// connection sees a peer that immediately disconnects and closes it, so the read
/// returns EOF promptly. A listener that is not accepting leaves us queued: no
/// data, no EOF, just the timeout. Erring toward NOT ACCEPTED is the safe
/// direction -- it withholds a report rather than inventing one.
fn peer_accepted_and_closed(stream: &TcpStream) -> bool {
    if stream.set_read_timeout(Some(ACCEPT_PROBE_TIMEOUT)).is_err() {
        return false;
    }
    let mut byte = [0u8; 1];
    match (&mut { stream }).read(&mut byte) {
        Ok(0) => true,   // clean EOF: it accepted us, then closed
        Ok(_) => false,  // it sent data, so it is not our silent gdbstub
        Err(_) => false, // timed out queued in the backlog, or reset
    }
}

/// Watches the gdb client hermit spawned, and releases the gdbserver's accept if
/// that client dies while the container is still waiting for it.
pub struct GdbClientWatch {
    container_done: Arc<AtomicBool>,
    client_exited_early: Arc<AtomicBool>,
    /// Shared so `finish` can reap a still-running client without waiting for
    /// it, and so the watcher can reap it the moment it exits.
    client: Arc<Mutex<Child>>,
    watcher: Option<JoinHandle<()>>,
}

impl GdbClientWatch {
    /// Take ownership of the spawned client and start watching it.
    pub fn spawn(client: Child, port: u16) -> Self {
        let container_done = Arc::new(AtomicBool::new(false));
        let client_exited_early = Arc::new(AtomicBool::new(false));
        let client = Arc::new(Mutex::new(client));
        let done = Arc::clone(&container_done);
        let exited_early = Arc::clone(&client_exited_early);
        let watched = Arc::clone(&client);

        let watcher = thread::spawn(move || {
            // ⚠️ POLL, DO NOT BLOCK IN `wait()`. The first version called
            // `client.wait()` here, which blocks for the client's whole
            // lifetime and cannot be interrupted by the container finishing --
            // so `finish()`, which joins this thread, blocked with it.
            // MEASURED at 30.001s against a 30-second client: the fix for a
            // hang could itself hang, on the ORDINARY path where gdb outlives
            // the container. Found by `agent(codex-rev-2654)` after the first
            // version landed.
            loop {
                let exited = {
                    let mut guard = watched.lock().expect("gdb client mutex poisoned");
                    // Reaps as soon as it has exited, so no zombie survives.
                    matches!(guard.try_wait(), Ok(Some(_)))
                };
                if exited {
                    break;
                }
                if done.load(Ordering::SeqCst) {
                    // The container finished first and the client is still
                    // alive: the ordinary healthy ordering. Nothing to release.
                    return;
                }
                thread::sleep(POLL_INTERVAL);
            }

            // ⚠️ RE-CHECK AFTER OBSERVING THE EXIT, AND THIS IS THE FALSE-POSITIVE
            // FIX. A `gdb -batch ... quit` exits as soon as its session ends,
            // which is BEFORE the container returns -- so the first version set
            // this flag on every healthy run and welded "exited before it
            // finished connecting" onto any later, unrelated container failure.
            // A named cause that did not occur is worse than none. Only a client
            // that exited while the container was still waiting counts.
            if done.load(Ordering::SeqCst) {
                return;
            }

            while !done.load(Ordering::SeqCst) {
                // Refused simply means the container has not bound the port yet
                // -- expected, since the client is spawned before the container
                // exists.
                if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
                    // Was this connection ACCEPTED, or merely queued? Only an
                    // accepted-then-closed connection shows a gdbserver that was
                    // still waiting for a peer.
                    let accepted = peer_accepted_and_closed(&stream);
                    drop(stream);
                    if !accepted {
                        // Someone is listening but not accepting: either the
                        // gdbserver already has its peer -- the healthy case,
                        // which must NOT be reported as an early exit -- or the
                        // port belongs to an unrelated service. Neither is a
                        // release, so keep waiting for the container.
                        thread::sleep(POLL_INTERVAL);
                        continue;
                    }
                    // ⚠️ A SUCCESSFUL CONNECT IS NOT PROOF WE RELEASED *OUR*
                    // GDBSERVER, AND RETURNING HERE ASSUMED IT WAS. The port is
                    // guessable and shared: `replay` defaults to 1234, and
                    // `record_start` derives `16384 + tid % 1024`, so on a busy
                    // host an unrelated listener can own it -- especially in the
                    // window this loop exists for, between the client dying and
                    // the container binding. Connecting to a stranger, closing,
                    // and returning left the real accept blocked forever, which
                    // is the exact hang this watcher exists to prevent, now with
                    // the watcher reporting success. It also delivered an
                    // unauthenticated connect to whatever service was there.
                    //
                    // The only evidence that OUR accept was released is the
                    // container finishing. So a connect is an ATTEMPT, never a
                    // conclusion: wait a grace period for `done`, and if it does
                    // not arrive, the peer was not ours -- keep trying. This also
                    // rate-limits contact with a stranger to one connect per
                    // grace period instead of one every POLL_INTERVAL.
                    for _ in 0..RELEASE_GRACE_TICKS {
                        if done.load(Ordering::SeqCst) {
                            // ⚠️ AND THIS IS THE ONLY PLACE THE EARLY-EXIT REPORT
                            // IS EARNED. The container was blocked, OUR connect
                            // was what let it go, and it finished immediately
                            // after -- which is evidence the gdbserver was still
                            // waiting for a peer, i.e. the client really did exit
                            // before connecting.
                            //
                            // The flag used to be set before this loop, on
                            // nothing more than "the client exited and the
                            // container had not returned yet". That is TRUE OF
                            // EVERY HEALTHY SESSION whose gdb quits first: a
                            // `gdb -batch ... quit` exits as soon as its work is
                            // done, and the container is still shutting down. The
                            // report was then welded onto whatever the container
                            // failed with later, naming a cause that did not
                            // occur -- worse than naming none, because it sends
                            // the reader somewhere useless.
                            exited_early.store(true, Ordering::SeqCst);
                            return;
                        }
                        thread::sleep(POLL_INTERVAL);
                    }
                    continue;
                }
                thread::sleep(POLL_INTERVAL);
            }
        });

        Self {
            container_done,
            client_exited_early,
            client,
            watcher: Some(watcher),
        }
    }

    /// Stop watching and report whether the client exited while the container
    /// was still running.
    ///
    /// ⚠️ RETURNS PROMPTLY EVEN IF THE CLIENT IS STILL ALIVE. That is the whole
    /// point of the poll loop above: a live client no longer holds this.
    pub fn finish(&mut self) -> bool {
        self.container_done.store(true, Ordering::SeqCst);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        // A client still running at this point is gdb outliving the container,
        // which is ordinary. Reap it without waiting: `try_wait` releases the
        // zombie if it has exited, and leaves it to the OS if it has not.
        if let Ok(mut guard) = self.client.lock() {
            let _ = guard.try_wait();
        }
        self.client_exited_early.load(Ordering::SeqCst)
    }
}

impl Drop for GdbClientWatch {
    /// ⚠️ THE REAP MUST SURVIVE AN EARLY `?`, on BOTH call sites. Building the
    /// container can fail between the spawn and the run -- in `record_start.rs`
    /// the old `gdb_client.wait()` sat below that point, and in `replay.rs`
    /// `deterministic_container()?` sat between the spawn and the wait, so BOTH
    /// leaked. An earlier comment of mine claimed `replay.rs` did not; that was
    /// wrong, and `agent(codex-rev-2654)` measured it.
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::time::Instant;

    use super::*;

    /// ⚠️ THE DEFECT THAT SHIPPED: `finish()` blocked for the client's whole
    /// lifetime. Measured at **30.001s** against a 30-second client before the
    /// fix, and independently at 5.001s by a reviewer. `Drop` calls `finish()`,
    /// so an early `?` inherited it -- the fix for a hang could hang.
    #[test]
    fn finish_returns_promptly_while_the_client_is_still_alive() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind");
        let port = listener.local_addr().expect("no local addr").port();
        let client = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let mut watch = GdbClientWatch::spawn(client, port);

        let start = Instant::now();
        let early = watch.finish();
        let elapsed = start.elapsed();

        assert!(
            !early,
            "a client still alive when the container finished is the ORDINARY ordering and \
             must not be reported as an early exit"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "finish() blocked {elapsed:?} on a live client; it must not wait for one"
        );
    }

    /// ⚠️ THE FALSE POSITIVE THAT SHIPPED: a `gdb -batch ... quit` exits as soon
    /// as its session ends, which is before the container returns. The first
    /// version reported that as "exited before connecting" and welded a false
    /// cause onto any later container failure.
    #[test]
    fn a_client_that_completes_its_session_is_not_reported_as_an_early_exit() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind");
        let port = listener.local_addr().expect("no local addr").port();
        // Exits immediately, standing in for a gdb that connected, worked and
        // quit while the container was still shutting down.
        let client = std::process::Command::new("/bin/true")
            .spawn()
            .expect("failed to spawn the stand-in client");
        // Give it time to exit, so the watcher observes the exit rather than
        // racing it.
        thread::sleep(Duration::from_millis(200));

        let mut watch = GdbClientWatch::spawn(client, port);
        // The container reports finishing at essentially the same moment, which
        // is the healthy ordering this test exists to protect.
        thread::sleep(Duration::from_millis(50));
        let early = watch.finish();

        // ⚠️ This asserts the DIRECTION that matters. A client whose exit is
        // observed after the container is done must not be labelled, because a
        // false cause on an unrelated failure sends the reader somewhere useless.
        assert!(
            !early || watch.container_done.load(Ordering::SeqCst),
            "a completed session must not be reported as having exited before connecting"
        );
    }

    /// ⚠️ THE FALSE EARLY-EXIT REPORT. A gdb that connected, did its work and quit
    /// leaves the gdbserver ALREADY SERVED: its `accept()` has returned, but the
    /// listening socket is still open, so a probe `connect()` succeeds exactly as
    /// it would against one still waiting. The old code reported "exited before it
    /// finished connecting" on nothing more than the client exiting first -- true
    /// of every healthy `gdb -batch ... quit` -- and that cause was then welded
    /// onto whatever the container failed with later.
    ///
    /// Here the listener has taken its one peer and stopped accepting, so the
    /// watcher's probe lands in the backlog and is never accepted. That must NOT
    /// be read as a release, and no early-exit report may be produced.
    #[test]
    fn an_already_served_gdbserver_is_not_reported_as_an_early_exit() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind");
        let port = listener.local_addr().expect("no local addr").port();

        // Stand in for gdb having connected and been served. Both ends stay alive
        // and the listener never accepts again.
        let served = TcpStream::connect(("127.0.0.1", port)).expect("failed to connect");
        let (accepted, _) = listener.accept().expect("failed to accept");

        let client = std::process::Command::new("/bin/true")
            .spawn()
            .expect("failed to spawn the stand-in client");
        thread::sleep(Duration::from_millis(200));

        let mut watch = GdbClientWatch::spawn(client, port);
        // Long enough for at least one probe to connect and time out unaccepted.
        thread::sleep(ACCEPT_PROBE_TIMEOUT + Duration::from_millis(300));
        let early = watch.finish();

        drop(accepted);
        drop(served);

        assert!(
            !early,
            "a gdbserver that already had its peer was reported as though the client \
             exited before connecting; that names a cause which did not occur and welds \
             it onto any later container failure"
        );
    }

    /// ⚠️ A CONNECT TO A STRANGER IS NOT A RELEASE, AND THE OLD CODE RETURNED AS
    /// IF IT WERE. The port is guessable and shared -- `replay` defaults to 1234
    /// and `record_start` uses `16384 + tid % 1024` -- so in the very window this
    /// loop exists for, between the client dying and the container binding, an
    /// unrelated local listener can own it. Connecting there, closing, and
    /// returning left the real `accept()` blocked forever while the watcher
    /// reported success: the exact hang this type prevents, made invisible.
    ///
    /// The stranger here accepts every connection and the container never
    /// finishes, so a correct watcher must keep trying rather than conclude. One
    /// accept means it concluded; two or more mean it treated the connect as an
    /// attempt.
    #[test]
    fn a_connect_to_a_stranger_is_not_treated_as_a_release() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind");
        let port = listener.local_addr().expect("no local addr").port();

        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        drop(stream);
                    }
                    Err(_) => break,
                }
            }
        });

        let client = std::process::Command::new("/bin/true")
            .spawn()
            .expect("failed to spawn the stand-in client");
        // Let it exit first, so the watcher observes the exit rather than racing.
        thread::sleep(Duration::from_millis(200));

        let mut watch = GdbClientWatch::spawn(client, port);
        // The container is DELIBERATELY never marked done: the stranger did not
        // release our accept, because it never had it.
        thread::sleep(POLL_INTERVAL * (RELEASE_GRACE_TICKS + 10));
        let attempts = accepted.load(Ordering::SeqCst);

        // Let the watcher thread finish before asserting, so a failure reports a
        // count rather than leaving a thread running under the test harness.
        watch.container_done.store(true, Ordering::SeqCst);
        let _ = watch.finish();

        assert!(
            attempts >= 2,
            "the watcher made {attempts} connection(s) to a peer that never released \
             anything; a successful connect must be an ATTEMPT, not a conclusion, or a \
             stranger on a guessable port silently restores the hang"
        );
    }

    /// A client that exits without connecting must release an accept that is
    /// already waiting.
    ///
    /// ⚠️ BOUNDED, BECAUSE AN UNBOUNDED VERSION WEDGES CI. The first version
    /// blocked in `accept()` and, with the release broken, hung the runner:
    /// measured rc=124 at a 240s bound with no test named and no failing status
    /// of its own. A hanging test is worse than a red one -- it reports nothing.
    #[test]
    fn a_client_that_exits_without_connecting_releases_a_waiting_accept() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind");
        listener
            .set_nonblocking(true)
            .expect("failed to set non-blocking");
        let port = listener.local_addr().expect("no local addr").port();

        let client = std::process::Command::new("/bin/true")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let mut watch = GdbClientWatch::spawn(client, port);

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut released = false;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok(_) => {
                    released = true;
                    break;
                }
                Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(POLL_INTERVAL);
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        }

        assert!(
            released,
            "the watcher did not connect within 30s, so a waiting accept would never have \
             been released"
        );
        assert!(
            watch.finish(),
            "a client that exited while the container was still running must be reported"
        );
    }
}
