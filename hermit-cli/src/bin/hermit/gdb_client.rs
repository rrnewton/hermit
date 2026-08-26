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

use std::net::TcpStream;
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

/// Context added to a container failure when the spawned client had already
/// exited, so the error names the cause rather than the symptom.
pub const CLIENT_EXITED_BEFORE_CONNECTING: &str = "the gdb client hermit spawned exited before it finished connecting to the \
     gdbserver, so the replay had no debugger to serve";

/// How often to retry the release connection while the client is gone and the
/// container is still running.
///
/// ⚠️ THIS IS NOT A TIMEOUT AND IT BOUNDS NOTHING. The loop it paces exits on the
/// container finishing or the connection succeeding, both facts; the interval
/// only decides how promptly a released accept is noticed. The port may not be
/// bound yet when the client dies — hermit spawns the client first — so the
/// release cannot be a single attempt.
const RELEASE_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// Watches the gdb client hermit spawned, and releases the gdbserver's accept if
/// that client dies while the container is still waiting for it.
///
/// Reaping the client is this type's job too: the previous code called
/// `gdb_client.wait()` AFTER the container result was propagated with `?`, so
/// every error path left an unreaped `gdb`. Owning the [`Child`] here means the
/// reap happens on every path, including the early returns.
pub struct GdbClientWatch {
    container_done: Arc<AtomicBool>,
    client_exited_early: Arc<AtomicBool>,
    watcher: Option<JoinHandle<()>>,
}

impl GdbClientWatch {
    /// Take ownership of the spawned client and start watching it.
    ///
    /// `port` is the gdbserver port the container is about to listen on. The
    /// watcher connects to it ONLY after observing the client exit, and only
    /// while the container is still running.
    pub fn spawn(mut client: Child, port: u16) -> Self {
        let container_done = Arc::new(AtomicBool::new(false));
        let client_exited_early = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&container_done);
        let exited_early = Arc::clone(&client_exited_early);

        let watcher = thread::spawn(move || {
            // Blocks until the client exits, and reaps it.
            let _ = client.wait();

            // The ordinary case: the session completed and the container
            // returned first, so gdb exiting is just gdb finishing. Nothing to
            // release, and nothing to report.
            if done.load(Ordering::SeqCst) {
                return;
            }

            // ⚠️ THE CLIENT IS GONE AND THE CONTAINER IS NOT. Whatever the
            // container is doing, no debugger will ever attach to it, so a
            // gdbserver blocked in accept() is blocked forever. One connection
            // releases it; the peer closing immediately is what tells the
            // gdbstub the session is over.
            exited_early.store(true, Ordering::SeqCst);
            while !done.load(Ordering::SeqCst) {
                // Refused simply means the container has not bound the port yet
                // — expected, because the client is spawned before the container
                // exists. Retry until it binds or the container finishes.
                if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
                    drop(stream);
                    return;
                }
                thread::sleep(RELEASE_RETRY_INTERVAL);
            }
        });

        Self {
            container_done,
            client_exited_early,
            watcher: Some(watcher),
        }
    }

    /// Stop watching, reap the client, and report whether it had exited while
    /// the container was still running.
    ///
    /// Call this once the container run has returned, BEFORE propagating its
    /// result, so a failure can be given the cause rather than the symptom.
    pub fn finish(&mut self) -> bool {
        self.container_done.store(true, Ordering::SeqCst);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        self.client_exited_early.load(Ordering::SeqCst)
    }
}

impl Drop for GdbClientWatch {
    /// ⚠️ THE REAP MUST SURVIVE AN EARLY `?`. Building the container can fail
    /// between the spawn and the run, and the original code's `gdb_client.wait()`
    /// sat below that point, so such a path left an orphan. Dropping this joins
    /// the watcher, which has waited on the client.
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    /// A client that exits without connecting must release an accept that is
    /// already waiting.
    ///
    /// This is the hang, in miniature: a listener with nobody coming. Without
    /// the watcher, `accept()` here never returns.
    #[test]
    fn a_client_that_exits_without_connecting_releases_a_waiting_accept() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        let client = std::process::Command::new("/bin/true")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let mut watch = GdbClientWatch::spawn(client, port);

        // Blocks until the watcher connects. The test hangs rather than fails if
        // the release is broken, which is the honest failure for this property:
        // the defect being fixed IS a hang.
        let (_stream, _peer) = listener.accept().expect("accept failed");

        assert!(
            watch.finish(),
            "the client exited while the container was still running, so that must be reported"
        );
    }

    /// The watcher must NOT report an early exit when the container finished
    /// first, or every healthy gdb session would be labelled a failure.
    #[test]
    fn a_client_that_outlives_the_container_is_not_reported_as_early() {
        // Bound but never accepted: the container "finishes" without the client
        // having exited, which is the ordinary ordering.
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        let client = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let mut watch = GdbClientWatch::spawn(client, port);

        // The container returns first. `finish` must not wait for a 30-second
        // client, and must not claim it exited early.
        let reported_early = {
            watch.container_done.store(true, Ordering::SeqCst);
            watch.client_exited_early.load(Ordering::SeqCst)
        };
        assert!(
            !reported_early,
            "a client still running when the container finished must not be reported as an \
             early exit"
        );

        // Tidy up the stand-in rather than leaving it for the suite's lifetime.
        watch.watcher.take();
        drop(listener);
    }
}
