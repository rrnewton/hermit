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

use std::net::SocketAddr;
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

/// How often the watcher asks whether the client has exited.
///
/// ⚠️ THIS EXISTS BECAUSE A BLOCKING `wait()` CANNOT BE INTERRUPTED, and that is
/// what made the first version of this file able to hang. Polling costs one
/// `waitpid(WNOHANG)` per interval and buys the ability to stop watching, which
/// a blocking wait does not sell at any price.
const CLIENT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Bound on ONE release-connect attempt.
///
/// ⚠️ THIS IS NOT A TIMEOUT ON ANYTHING THAT MATTERS. The loop retries, and it
/// exits on the container finishing or on a connect succeeding -- both facts.
/// This only stops a single `connect` to a saturated accept queue from stalling
/// for minutes on the path `finish()` now joins. Localhost either refuses
/// instantly or completes instantly; a stall here means the peer is not the one
/// we are looking for, so retrying is the right response to it.
const RELEASE_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

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
            // ⚠️ POLL, DO NOT BLOCK. This was `client.wait()`, which blocks until
            // the client exits and cannot be woken by `container_done`. That made
            // `finish()` — which joined this thread — unable to return whenever
            // gdb outlived the container, WHICH IS THE ORDINARY ORDERING. A
            // watcher written to stop a hang could hang, exactly when the thing
            // it watches for happened.
            //
            // `try_wait` reaps on the same call, so the client is still reaped on
            // every path; the loop simply also gets to notice the container.
            loop {
                match client.try_wait() {
                    // Exited and reaped. Fall through to the report/release
                    // decision below.
                    Ok(Some(_)) => break,
                    Ok(None) => {}
                    // We can no longer observe this child, so we can say nothing
                    // about it. Claiming an early exit here would be inventing a
                    // cause; leave the flag false and let the container speak.
                    Err(_) => return,
                }
                if done.load(Ordering::SeqCst) {
                    // ⚠️ THE SUCCESS-PATH WAIT, RESTORED HERE. The container
                    // finished while the client is still alive: not an early
                    // exit, nothing to release. Before the watcher existed, both
                    // call sites ended with `let _ = gdb_client.wait();` at
                    // exactly this point, so hermit did not return while the gdb
                    // it spawned was still running. Two rewrites of this file
                    // dropped that -- the first silently, the second by detaching
                    // -- and neither argued for the change.
                    //
                    // Waiting HERE rather than in `finish()` is what keeps it
                    // compatible with the defect this file exists to prevent: the
                    // release decision is already made (there is nothing to
                    // release), so nothing downstream is gated on the client. See
                    // `finish` and `drop` for which of the two joins.
                    let _ = client.wait();
                    return;
                }
                thread::sleep(CLIENT_POLL_INTERVAL);
            }

            // ⚠️ RE-READ `done` AT THE MOMENT OF THE DECISION, NOT BEFORE IT.
            // A gdb that finishes normally a moment before the container returns
            // is a HEALTHY teardown. The first version set the flag on the
            // strength of a `done` read taken at client-exit time and then
            // attached "the client exited before connecting" to whatever error
            // the container produced — a false cause on a real failure, which
            // sends the next reader hunting a bug that does not exist.
            if done.load(Ordering::SeqCst) {
                return;
            }

            // ⚠️ THE CLIENT IS GONE AND THE CONTAINER IS NOT. Whatever the
            // container is doing, no debugger will ever attach to it, so a
            // gdbserver blocked in accept() is blocked forever. One connection
            // releases it; the peer closing immediately is what tells the
            // gdbstub the session is over.
            // ⚠️ THE FLAG IS SET WHEN WE ACTUALLY RELEASE A BLOCKED ACCEPT, NOT
            // WHEN THE CLIENT EXITS. This is the correction that closes defect 2,
            // and re-reading `done` a moment later does NOT close it: if gdb
            // genuinely exits a moment before the container returns — a HEALTHY
            // teardown — then `done` is false at client-exit time and false again
            // an instant later, so any check taken at that moment flags it.
            //
            // "Did the client exit?" is the wrong question. "Was the container
            // still waiting for a client that will never come?" is the right one,
            // and a successful release connect is the evidence for it: it means
            // an accept was pending with nobody coming. If instead the container
            // finishes on its own while we retry, the loop below exits on `done`
            // and we say nothing — which is exactly the healthy case that used to
            // be reported as "exited before connecting".
            while !done.load(Ordering::SeqCst) {
                // ⚠️ THIS CONNECT IS UNAUTHENTICATED AND THE PORT IS GUESSABLE:
                // the call sites use `16384 + gettid() % 1024`. If the container's
                // listener is gone and an unrelated process has taken that port,
                // this connects to a stranger and drops it.
                //
                // ⚠️ AND MOVING THE FLAG ONTO THIS CONNECT RAISED THE STAKES, which
                // is new: a foreign listener now also produces a FALSE "the client
                // exited before connecting" line, not merely a stray connection.
                // Judged worth it — the alternative was reporting that false cause
                // on EVERY healthy teardown rather than on a port collision — but
                // it is a real trade and not a free win. The fix is to identify
                // the listener rather than to guess the port, which changes an
                // interface; tracked as `gdb_watcher_release_probe`.
                //
                // Refused simply means the container has not bound the port yet
                // — expected, because the client is spawned before the container
                // exists. Retry until it binds or the container finishes.
                // ⚠️ AND IT IS BOUNDED BECAUSE `finish()` NOW JOINS THIS THREAD.
                // A bare `TcpStream::connect` has no timeout: against a listener
                // whose accept queue is full the SYN is dropped and the call
                // stalls for minutes. While this thread was detached that only
                // delayed a background thread; a join puts it on hermit's own
                // return path, so the stall has to be bounded. This is a bound on
                // ONE CONNECT ATTEMPT inside a loop that already retries -- not a
                // timeout on the run, and not on the accept.
                let peer = SocketAddr::from(([127, 0, 0, 1], port));
                if let Ok(stream) = TcpStream::connect_timeout(&peer, RELEASE_CONNECT_TIMEOUT) {
                    drop(stream);
                    // We released a pending accept, so the container WAS waiting
                    // for a client that had already gone. Now the report is earned.
                    exited_early.store(true, Ordering::SeqCst);
                    return;
                }
                thread::sleep(RELEASE_RETRY_INTERVAL);
            }
            // `done` won the race: the container finished under its own power, so
            // the client exiting first was an ordinary teardown. Say nothing.
        });

        Self {
            container_done,
            client_exited_early,
            watcher: Some(watcher),
        }
    }

    /// Stop watching, WAIT FOR THE CLIENT, reap it, and report whether it had
    /// exited while the container was still running.
    ///
    /// Call this once the container run has returned, BEFORE propagating its
    /// result, so a failure can be given the cause rather than the symptom.
    ///
    /// ⚠️ THE WAIT IS A RESTORATION, NOT AN ADDITION, AND IT IS THE POINT OF THIS
    /// CHANGE. Before the watcher existed, both call sites ended with `let _ =
    /// gdb_client.wait();` — hermit did not return while the gdb it had spawned
    /// was still running. The first watcher deleted that silently; the rewrite
    /// that followed replaced the join with a detach, which deletes it again by a
    /// different route. Neither argued for the change, and it is observable: the
    /// shell prompt comes back with gdb still writing to the same terminal, and
    /// the client reparents to init.
    ///
    /// ⚠️ AND THE JOIN CANNOT REINTRODUCE THE BLOCK IT REPLACED, which is the
    /// distinction the whole file turns on. The old block was a wait taken BEFORE
    /// the release decision: the watcher's first act was `client.wait()`, so it
    /// could not notice the container, could not release the accept, and
    /// `finish()` inherited all of it — on error paths too, via `Drop`. The wait
    /// this joins is taken AFTER that decision, on the branch where the container
    /// has already finished and there is by construction nothing to release. It
    /// gates nothing.
    ///
    /// The flag read is unchanged in meaning: the watcher only sets it while
    /// `done` is false, and re-reads `done` at the moment of decision, so a
    /// client exiting as the container returns is still treated as the healthy
    /// teardown it is.
    ///
    /// ⚠️ `Drop` DELIBERATELY DOES NOT CALL THIS. See below.
    pub fn finish(&mut self) -> bool {
        self.container_done.store(true, Ordering::SeqCst);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        self.client_exited_early.load(Ordering::SeqCst)
    }

    /// Stop watching WITHOUT waiting for anything.
    fn abandon(&mut self) {
        self.container_done.store(true, Ordering::SeqCst);
        // Detach. Dropping the handle does not stop the thread; it stops US
        // waiting on it, which is the whole point.
        self.watcher.take();
    }
}

impl Drop for GdbClientWatch {
    /// ⚠️ THE REAP MUST SURVIVE AN EARLY `?`. Building the container can fail
    /// between the spawn and the run, and the original code's `gdb_client.wait()`
    /// sat below that point, so such a path left an orphan. Dropping this signals
    /// the watcher, which owns the client and reaps it.
    ///
    /// ⚠️ AND IT DETACHES WHERE `finish` JOINS, WHICH IS THE WHOLE DIFFERENCE
    /// BETWEEN THE TWO PATHS. `finish` runs when the container has returned, so
    /// waiting for the client is what hermit always did. `Drop` runs when it has
    /// NOT — an early `?` means there may be no container coming at all, so a
    /// client still trying to connect to a port nobody will ever bind would never
    /// exit, and joining here would hang the error exit. That is the original
    /// defect rebuilt one level down, on the paths that were merely returning an
    /// error. The detached watcher still reaps the client; nobody waits for it.
    fn drop(&mut self) {
        self.abandon();
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::time::Instant;

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

        // ⚠️ BOUNDED, BECAUSE A HANGING TEST IS WORSE THAN A RED ONE. The first
        // version called the blocking `accept()` and argued that hanging was "the
        // honest failure for this property, since the defect under test IS a
        // hang". That reasoning is wrong in a suite: a red test names itself in
        // one line, while a wedged one consumes the whole run's budget and is
        // reported as a timeout somewhere else entirely.
        listener
            .set_nonblocking(true)
            .expect("failed to set the test listener non-blocking");
        let deadline = Instant::now() + Duration::from_secs(30);
        let accepted = loop {
            match listener.accept() {
                Ok(pair) => break Some(pair),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        };
        assert!(
            accepted.is_some(),
            "the watcher did not release a waiting accept within 30s after the client exited; \\
             the release is broken and the gdbserver would block forever"
        );

        assert!(
            watch.finish(),
            "the client exited while the container was still running, so that must be reported"
        );
    }

    /// The watcher must NOT report an early exit when the container finished
    /// first, or every healthy gdb session would be labelled a failure -- and
    /// `finish()` must WAIT for that client rather than walking away from it.
    ///
    /// ⚠️ THIS TEST ASSERTED THE OPPOSITE ONE COMMIT AGO, and the reversal is the
    /// substance of this change. It read:
    ///
    /// ```text
    /// assert!(elapsed < Duration::from_secs(2),
    ///     "finish() took {elapsed:?}; it must not wait on a client that outlives the container");
    /// ```
    ///
    /// That encoded "hermit must not wait for the gdb it spawned" as a
    /// requirement. It never was one: before this file existed, both call sites
    /// ended with `let _ = gdb_client.wait();` and hermit waited. The requirement
    /// the assertion was reaching for is real, but it belongs on `Drop` -- see
    /// `dropping_a_watch_never_waits_for_the_client` -- because the defect was a
    /// wait taken BEFORE the release decision, on paths that were merely
    /// returning an error. Telling those two waits apart is the whole change.
    #[test]
    fn finish_waits_for_a_client_that_outlives_the_container() {
        // Bound but never accepted: the container "finishes" without the client
        // having exited, which is the ordinary ordering.
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        // Long enough that "did it wait?" is unambiguous, short enough that a
        // stray cannot outlive the suite. An earlier version left `sleep 30`
        // alive after `cargo test` returned 0.
        let client = std::process::Command::new("/bin/sleep")
            .arg("1")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let mut watch = GdbClientWatch::spawn(client, port);

        // ⚠️ THIS CALLS `finish()`, WHICH IS THE POINT. An earlier version poked
        // `container_done` and read `client_exited_early` by hand and then
        // detached the thread -- because calling `finish()` would have hit the
        // blocking join and hung. So the test AVOIDED the defect instead of
        // catching it. Going through the real entry point is what makes this a
        // regression test rather than a description.
        let started = Instant::now();
        let reported_early = watch.finish();
        let elapsed = started.elapsed();

        assert!(
            !reported_early,
            "a client still running when the container finished must not be reported as an early exit"
        );
        assert!(
            elapsed >= Duration::from_millis(900),
            "finish() returned after {elapsed:?}, before a 1-second client could have exited; \
             hermit is not waiting for the gdb it spawned"
        );
        drop(listener);
    }

    /// ⚠️ THE PROMPTNESS GUARANTEE, ON THE PATH THAT ACTUALLY NEEDS IT. An early
    /// `?` between the spawn and the run drops the watch without a container ever
    /// having existed, so a client still trying to connect to a port nobody will
    /// bind would never exit. The original defect was a `Drop` that inherited a
    /// blocking wait and hung paths that were merely returning an error --
    /// measured at 30.001s. Restoring the success-path wait must not bring that
    /// back with it, and this is what says so.
    #[test]
    fn dropping_a_watch_never_waits_for_the_client() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        let client = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let pid = client.id();
        let watch = GdbClientWatch::spawn(client, port);

        let started = Instant::now();
        drop(watch);
        let elapsed = started.elapsed();

        // The detached watcher owns the client and reaps it. Kill the stand-in so
        // the suite leaves nothing running: a reviewer measured a `/bin/sleep 30`
        // alive after `cargo test` returned 0, reparented to the user subreaper,
        // and a leak like that is invisible in a green run.
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();

        assert!(
            elapsed < Duration::from_secs(5),
            "dropping the watch blocked {elapsed:?} on a live client; the error path must never \
             wait for one"
        );
        drop(listener);
    }

    /// A client that exits AFTER the container finished is a healthy teardown and
    /// must not be reported as "exited before connecting".
    ///
    /// ⚠️ THIS IS DEFECT 2, AND IT IS THE INVERSE OF A HANG: a correct outcome
    /// described as a failure. The first version read `done` only once, after a
    /// blocking `wait()`, so ANY client that exited before the container returned
    /// was flagged — and the flag adds a context line saying the client never
    /// connected. A false cause attached to a real failure sends the next reader
    /// hunting a bug that does not exist.
    #[test]
    fn a_client_that_exits_after_the_container_finished_is_not_reported_as_early() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        // Still alive when the container finishes, gone shortly after.
        let client = std::process::Command::new("/bin/sleep")
            .arg("1")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let mut watch = GdbClientWatch::spawn(client, port);

        // The container returns while the client is still running.
        let reported_early = watch.finish();
        assert!(
            !reported_early,
            "reported early while the client was still alive"
        );

        // Now let the client exit, well after `done` was published. The watcher is
        // detached and still polling; it must observe the exit and stay silent.
        thread::sleep(Duration::from_millis(2500));
        assert!(
            !watch.client_exited_early.load(Ordering::SeqCst),
            "a client that exited AFTER the container finished was reported as having exited \
             before connecting -- a healthy teardown described as a failed connect"
        );
        drop(listener);
    }

    /// The client exits FIRST, and the container then finishes on its own. There
    /// was no blocked accept to release, so this is a healthy teardown and must
    /// not be reported as "exited before connecting".
    ///
    /// ⚠️ THIS IS THE CASE RE-READING `done` CANNOT CATCH, and pinning it is why
    /// the flag moved. The client is gone before the container returns, so a
    /// check taken at client-exit time sees `done == false` — and so does a check
    /// taken an instant later. Only asking "did we have to release an accept?"
    /// distinguishes a stranded container from an ordinary shutdown.
    ///
    /// No listener is bound on `port`, so the release connect can never succeed:
    /// exactly the shape of a container that needed no rescuing.
    #[test]
    fn a_client_that_exits_first_but_strands_nobody_is_not_reported_as_early() {
        // Bind and immediately drop, so the port is plausible but nothing listens.
        let port = {
            let l = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind");
            l.local_addr().expect("no local addr").port()
        };

        let client = std::process::Command::new("/bin/true")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let mut watch = GdbClientWatch::spawn(client, port);

        // Let the watcher observe the exit and spin on the release a few times.
        thread::sleep(Duration::from_millis(200));

        // The container now finishes under its own power.
        assert!(
            !watch.finish(),
            "a client that exited without stranding the container was reported as having \
             exited before connecting -- a healthy teardown described as a failed connect"
        );
        thread::sleep(Duration::from_millis(100));
        assert!(
            !watch.client_exited_early.load(Ordering::SeqCst),
            "the flag was set after the fact for a container that was never blocked"
        );
    }
}
