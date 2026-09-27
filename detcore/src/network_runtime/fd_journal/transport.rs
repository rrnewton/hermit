//! Existing controller/Inbox transport owns each request through cancellation.
use std::io;

use super::super::accepted_controller::Controller;
use super::super::accepted_controller::Effect;
use super::super::accepted_provider::Reply;
use super::super::accepted_provider::Request;
use super::super::accepted_transport::ObservationReceipt;
use super::History;
use crate::network_replay::NetworkStreamOwner;
#[derive(Debug)]
struct Pending {
    ordinal: u64,
    owner: NetworkStreamOwner,
}
#[derive(Debug, Default)]
pub(in crate::network_runtime) struct Journal {
    pub(super) history: History,
    pending: Option<Pending>,
    retired: Option<ObservationReceipt>,
    last_owner: Option<NetworkStreamOwner>,
    final_request: Option<u64>,
    finished: bool,
    failure: Option<String>,
}
impl Journal {
    pub(in crate::network_runtime) fn history(&self) -> &History {
        &self.history
    }
    /// No scheduler/engine/FD-table/socket-control lock may span this wait.
    pub(in crate::network_runtime) async fn through(
        &mut self,
        controller: &Controller,
        owner: NetworkStreamOwner,
        end: u64,
    ) -> io::Result<()> {
        if let Some(error) = &self.failure {
            return Err(io::Error::other(error.clone()));
        }
        let result = self.read_through(controller, owner, end).await;
        if let Err(error) = &result {
            self.failure = Some(error.to_string());
        }
        result
    }
    async fn read_through(
        &mut self,
        controller: &Controller,
        owner: NetworkStreamOwner,
        end: u64,
    ) -> io::Result<()> {
        if end == 0 || self.finished || self.final_request.is_some() {
            return Err(io::Error::other("invalid journal observation phase"));
        }
        while self.history.next()? <= end {
            if self.history.rows.len() >= super::MAX_RETAINED {
                return Err(io::Error::other("unpublished journal capacity exhausted"));
            }
            let ordinal = self.history.next()?;
            let pending = self.pending.get_or_insert(Pending { ordinal, owner });
            if pending.ordinal != ordinal {
                return Err(io::Error::other("journal cursor changed during recovery"));
            }
            let sequence = controller.prepare(
                Effect::FdObservation(ordinal),
                pending.owner,
                &Request::AwaitFdEvent {
                    sequence: ordinal,
                    acknowledged: self.retired.clone(),
                },
                || Ok(vec![]),
            )?;
            let reply = controller.response(sequence).await?;
            let bytes = serde_json::to_vec(&reply)?;
            let Reply::FdJournal {
                provider,
                status,
                event,
            } = reply
            else {
                return Err(io::Error::other("journal response changed kind"));
            };
            // Keep even rejected partial raw rows in run-owned storage. The
            // controller/Inbox also retains original status/errno bytes.
            let valid_calls = provider.status.returned == 0
                && provider.raw.fatal == 0
                && status.status.returned == 0
                && event.status.returned == 0;
            self.history.retain(status.raw, event.raw)?;
            if !valid_calls {
                return Err(io::Error::other(
                    "journal read failed; raw evidence retained",
                ));
            }
            self.last_owner = Some(pending.owner);
            self.pending = None;
            self.retired = Some(controller.retire_fd_observation(ordinal, sequence, &bytes)?);
        }
        Ok(())
    }
    pub(in crate::network_runtime) async fn finish_after_backend(
        &mut self,
        controller: &Controller,
    ) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        if self.pending.is_some() || self.failure.is_some() {
            return Err(io::Error::other(
                "unresolved journal request at backend terminal",
            ));
        }
        if let Some(receipt) = &self.retired {
            let owner = self
                .last_owner
                .ok_or_else(|| io::Error::other("journal lacks authenticated origin"))?;
            let sequence = match self.final_request {
                Some(sequence) => sequence,
                None => {
                    let sequence = controller.prepare(
                        Effect::FdObservationRetirement(receipt.sequence),
                        owner,
                        &Request::RetireFdObservation {
                            receipt: receipt.clone(),
                        },
                        || Ok(vec![]),
                    )?;
                    self.final_request = Some(sequence);
                    sequence
                }
            };
            if !matches!(controller.response(sequence).await?, Reply::Retired) {
                return Err(io::Error::other(
                    "journal final retirement changed response",
                ));
            }
        }
        self.retired = None;
        self.finished = true;
        // Historical rows remain run-owned. This does not certify semantic
        // drain, complete physical history or safe release of socket custody.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;

    use super::super::super::accepted_provider::CallStatus;
    use super::super::super::accepted_provider::Observation;
    use super::super::super::accepted_provider_ffi as ffi;
    use super::super::super::accepted_transport::AcceptedSession;
    use super::super::super::accepted_transport::Received;
    use super::*;
    #[test]
    fn cancelled_journal_wait_recovers_exact_request_and_final_retirement() {
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
        let controller = Controller::new(unsafe { OwnedFd::from_raw_fd(fds[0]) }, [1; 16]).unwrap();
        let mut peer =
            AcceptedSession::new(unsafe { OwnedFd::from_raw_fd(fds[1]) }, [1; 16]).unwrap();
        let thread = crate::types::DetTid::from_raw(31);
        let owner = NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        let mut journal = Journal::default();
        let mut cx = Context::from_waker(Waker::noop());
        {
            let mut waiting = Box::pin(journal.through(&controller, owner, 1));
            assert!(waiting.as_mut().poll(&mut cx).is_pending());
        }
        controller.drive_once().unwrap();
        let Some(Received::Request(sequence)) = peer.try_receive().unwrap() else {
            panic!("missing exact request")
        };
        assert_eq!(sequence, 1);
        let (request, rights, _) = peer.retained_request(sequence).unwrap();
        assert_eq!(request.owner, Some(owner));
        assert!(rights.is_empty());
        assert!(matches!(
            serde_json::from_slice::<Request>(&request.body).unwrap(),
            Request::AwaitFdEvent {
                sequence: 1,
                acknowledged: None
            }
        ));
        let status = || CallStatus {
            operation: "read".into(),
            returned: 0,
            errno: None,
        };
        let body = serde_json::to_vec(&Reply::FdJournal {
            provider: Observation {
                status: status(),
                raw: ffi::Status::default().into(),
            },
            status: Observation {
                status: status(),
                raw: ffi::FdStatus {
                    next_event: 1,
                    next_file: 1,
                    ..Default::default()
                }
                .into(),
            },
            event: Observation {
                status: status(),
                raw: ffi::FdEvent {
                    sequence: 1,
                    kind: 7,
                    task: 10,
                    task_start: 11,
                    file: 1,
                    fd: -1,
                    complete: 1,
                    ..Default::default()
                }
                .into(),
            },
        })
        .unwrap();
        peer.begin_observation(sequence).unwrap();
        peer.finish_observation(sequence, body.clone()).unwrap();
        peer.acknowledge_command_completion(sequence, |_, raw| {
            assert_eq!(raw, body);
            Ok(b"exact C ACK owned".to_vec())
        })
        .unwrap();
        assert!(peer.try_reply(sequence).unwrap());
        controller.drive_once().unwrap();
        {
            let mut recovered = Box::pin(journal.through(&controller, owner, 1));
            assert!(matches!(
                recovered.as_mut().poll(&mut cx),
                Poll::Ready(Ok(()))
            ));
        }
        assert_eq!(journal.history.rows.len(), 1);
        assert!(journal.pending.is_none());
        assert!(peer.try_receive().unwrap().is_none());
        {
            let mut finalizing = Box::pin(journal.finish_after_backend(&controller));
            assert!(finalizing.as_mut().poll(&mut cx).is_pending());
        }
        controller.drive_once().unwrap();
        let Some(Received::Request(final_sequence)) = peer.try_receive().unwrap() else {
            panic!("missing final retirement")
        };
        let (request, _, _) = peer.retained_request(final_sequence).unwrap();
        let Request::RetireFdObservation { receipt } =
            serde_json::from_slice(&request.body).unwrap()
        else {
            panic!("wrong final operation")
        };
        assert_eq!(receipt.sequence, sequence);
        assert_eq!(receipt.body, body);
        assert!(receipt.fd_journal);
        peer.retire_incoming_observation(&receipt).unwrap();
        peer.dispatch(final_sequence, |_, _| {
            Ok(serde_json::to_vec(&Reply::Retired).unwrap())
        })
        .unwrap();
        assert!(peer.try_reply(final_sequence).unwrap());
        controller.drive_once().unwrap();
        {
            let mut recovered = Box::pin(journal.finish_after_backend(&controller));
            assert!(matches!(
                recovered.as_mut().poll(&mut cx),
                Poll::Ready(Ok(()))
            ));
        }
        assert!(journal.finished);
        assert_eq!(journal.history.rows.len(), 1);
        assert!(controller.quiescent().unwrap());
    }
}
