//! Native transport progress owned by the controller, outside backend futures.

use std::io;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Instant;

use super::accepted_controller::Controller;

#[derive(Debug)]
pub(super) struct Driver {
    stop: Arc<AtomicBool>,
    done: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<Result<(), String>>>,
    outcome: Option<Result<(), String>>,
    join_failure: Option<String>,
}

impl Driver {
    /// Called only after Container clone. The callback owns the run's custody,
    /// so a lost final RPC cannot destroy pending collection or its descriptors.
    pub(super) fn start(
        controller: Arc<Controller>,
        retain: impl Fn() -> io::Result<()> + Send + 'static,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let thread_stop = stop.clone();
        let thread_done = done.clone();
        let thread = std::thread::Builder::new()
            .name("accepted-provider-io".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    while !thread_stop.load(Ordering::Acquire) {
                        let progress = controller.drive_once();
                        // Retain any known response and any transport failure
                        // before terminating. A failed provider result is data.
                        retain()?;
                        progress?;
                    }
                    retain()
                }));
                let outcome = match result {
                    Ok(result) => result.map_err(|error| error.to_string()),
                    Err(_) => Err("accepted native driver panicked; custody unresolved".into()),
                };
                if let Err(error) = &outcome {
                    controller.fail(&io::Error::other(error.clone()));
                }
                *thread_done.0.lock().unwrap() = true;
                thread_done.1.notify_all();
                outcome
            })?;
        Ok(Self {
            stop,
            done,
            thread: Some(thread),
            outcome: None,
            join_failure: None,
        })
    }

    /// Stop only after the caller has drained exact retained requests. Failure
    /// keeps this handle and the original deadline; Drop is not a join receipt.
    pub(super) fn stop_and_join(&mut self, deadline: Instant) -> io::Result<()> {
        self.stop_and_join_observed(deadline, Instant::now)
    }

    fn stop_and_join_observed(
        &mut self,
        deadline: Instant,
        observed: impl FnOnce() -> Instant,
    ) -> io::Result<()> {
        if let Some(error) = &self.join_failure {
            return Err(io::Error::other(error.clone()));
        }
        if let Some(result) = &self.outcome {
            return result.clone().map_err(io::Error::other);
        }
        self.stop.store(true, Ordering::Release);
        let mut done = self.done.0.lock().unwrap();
        while !self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| io::Error::other("accepted native driver join deadline"))?;
            done = self
                .done
                .1
                .wait_timeout(done, remaining.min(std::time::Duration::from_millis(50)))
                .unwrap()
                .0;
        }
        drop(done);
        if Instant::now() > deadline {
            return Err(io::Error::other("accepted native driver join deadline"));
        }
        let result = self
            .thread
            .take()
            .ok_or_else(|| io::Error::other("accepted driver join handle missing"))?
            .join()
            .unwrap_or_else(|_| Err("accepted native driver join panic".into()));
        self.outcome = Some(result.clone());
        // is_finished is a readiness hint: joining and observing completion
        // must also fit the original deadline. Keep the worker's first result
        // separately, including a known failure followed by a late join.
        if observed() > deadline {
            let error = "accepted native driver completion observed after deadline".to_owned();
            self.join_failure = Some(error.clone());
            return Err(io::Error::other(error));
        }
        result.map_err(io::Error::other)
    }

    /// Whether the worker thread was actually joined and its outcome retained.
    #[cfg(test)]
    pub(super) fn joined(&self) -> bool {
        self.thread.is_none() && self.outcome.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::sync::atomic::AtomicUsize;
    use std::task::Context;
    use std::task::Waker;
    use std::time::Duration;

    use super::super::accepted_controller::Effect;
    use super::super::accepted_provider::Reply;
    use super::super::accepted_provider::Request;
    use super::super::accepted_transport::AcceptedSession;
    use super::super::accepted_transport::Received;
    use super::*;
    use crate::network_replay::NetworkAcceptLeaseId;
    use crate::network_replay::NetworkStreamOwner;

    fn pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }
    fn owner() -> NetworkStreamOwner {
        let thread = crate::types::DetTid::from_raw(91);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }
    fn wait(deadline: Instant, mut ready: impl FnMut() -> bool) {
        while !ready() {
            assert!(
                Instant::now() < deadline,
                "native transport control deadline"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn event(value: u32) -> OwnedFd {
        let fd = unsafe { libc::eventfd(value, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        assert!(fd >= 0);
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    #[test]
    fn accepted_native_driver_finishes_without_final_waiter_or_tokio_runtime() {
        let deadline = Instant::now() + Duration::from_secs(2);
        let (endpoint, peer) = pair();
        let controller = Arc::new(Controller::new(endpoint, [7; 16]).unwrap());
        let mut service = AcceptedSession::new(peer, [7; 16]).unwrap();
        let transferred = AtomicUsize::new(0);
        let key = Effect::Match(NetworkAcceptLeaseId(4));
        let sequence = controller
            .prepare(key, owner(), &Request::ResolveAccepted, || {
                transferred.fetch_add(1, Ordering::SeqCst);
                Ok(vec![event(7), event(11)])
            })
            .unwrap();
        let mut response = Box::pin(controller.response(sequence));
        assert!(
            response
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(response); // No executor or sibling remains to drive this request.
        let mut driver = Driver::start(controller.clone(), || Ok(())).unwrap();
        let mut requests = 0;
        wait(deadline, || match service.try_receive().unwrap() {
            Some(Received::Request(received)) => {
                assert_eq!(received, sequence);
                service
                    .dispatch(received, |envelope, rights| {
                        requests += 1;
                        assert_eq!(envelope.owner, Some(owner()));
                        assert_eq!(envelope.accept, Some(NetworkAcceptLeaseId(4)));
                        assert_eq!(rights.len(), 2);
                        for (right, expected) in rights.iter().zip([7u64, 11]) {
                            assert_ne!(
                                unsafe { libc::fcntl(right.as_raw_fd(), libc::F_GETFD) }
                                    & libc::FD_CLOEXEC,
                                0
                            );
                            let mut value = 0u64;
                            assert_eq!(
                                unsafe {
                                    libc::read(
                                        right.as_raw_fd(),
                                        (&mut value as *mut u64).cast(),
                                        8,
                                    )
                                },
                                8
                            );
                            assert_eq!(value, expected);
                        }
                        Ok(serde_json::to_vec(&Reply::Retired).unwrap())
                    })
                    .unwrap();
                assert!(service.try_reply(received).unwrap());
                true
            }
            None => false,
            _ => panic!("wrong peer message"),
        });
        wait(deadline, || {
            controller.retained_response(sequence).unwrap().is_some()
        });
        assert_eq!(
            controller
                .prepare(key, owner(), &Request::ResolveAccepted, || panic!(
                    "duplicate rights transfer"
                ))
                .unwrap(),
            sequence
        );
        let changed = NetworkStreamOwner {
            mm: owner().mm.for_exec(owner().thread),
            ..owner()
        };
        assert!(
            controller
                .prepare(key, changed, &Request::ResolveAccepted, || panic!(
                    "wrong owner transferred rights"
                ))
                .is_err()
        );
        assert_eq!(requests, 1);
        assert_eq!(transferred.load(Ordering::SeqCst), 1);
        assert!(service.try_receive().unwrap().is_none());
        assert!(controller.quiescent().unwrap());
        driver.stop_and_join(deadline).unwrap();
        assert!(driver.thread.is_none());
    }

    #[test]
    fn accepted_native_driver_retains_peer_failure_after_waiter_drop() {
        let deadline = Instant::now() + Duration::from_secs(2);
        let (endpoint, peer) = pair();
        let controller = Arc::new(Controller::new(endpoint, [8; 16]).unwrap());
        let sequence = controller
            .prepare(
                Effect::Observation(1),
                owner(),
                &Request::ReadStatus,
                || Ok(vec![]),
            )
            .unwrap();
        let mut response = Box::pin(controller.response(sequence));
        assert!(
            response
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(response);
        drop(peer);
        let mut driver = Driver::start(controller.clone(), || Ok(())).unwrap();
        wait(deadline, || controller.retained_response(sequence).is_err());
        assert!(controller.quiescent().is_err());
        assert!(driver.stop_and_join(deadline).is_err());
        assert!(driver.thread.is_none());
    }

    #[test]
    fn accepted_native_driver_retains_late_join_separately_from_worker_result() {
        for failed in [false, true] {
            let deadline = Instant::now() + Duration::from_secs(2);
            let (endpoint, peer) = pair();
            let controller = Arc::new(Controller::new(endpoint, [9; 16]).unwrap());
            let mut peer = Some(peer);
            if failed {
                controller
                    .prepare(
                        Effect::Observation(1),
                        owner(),
                        &Request::ReadStatus,
                        || Ok(vec![]),
                    )
                    .unwrap();
                drop(peer.take());
            }
            let mut driver = Driver::start(controller, || Ok(())).unwrap();
            if !failed {
                driver.stop.store(true, Ordering::Release);
            }
            wait(deadline, || driver.thread.as_ref().unwrap().is_finished());
            // The observation seam supplies the same final clock read production
            // performs, without relying on a sub-millisecond scheduling race.
            let error = driver
                .stop_and_join_observed(deadline, || deadline + Duration::from_nanos(1))
                .unwrap_err();
            assert!(error.to_string().contains("observed after deadline"));
            assert_eq!(driver.outcome.as_ref().unwrap().is_err(), failed);
            assert!(driver.thread.is_none());
            assert!(
                driver
                    .stop_and_join(deadline + Duration::from_secs(10))
                    .is_err()
            );
        }
    }
}
