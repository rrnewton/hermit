// The failed-backend caller must send the same final observation retirement as
// successful cleanup, while the original native Driver still owns transport.
mod failed_backend_retirement {
    use std::io;
    use std::time::Duration;
    use std::time::Instant;

    use super::*;
    use crate::network_runtime::accepted_provider::CallStatus;
    use crate::network_runtime::accepted_provider::Observation;
    use crate::network_runtime::accepted_provider::Reply;
    use crate::network_runtime::accepted_provider::Request;
    use crate::network_runtime::accepted_provider_ffi as ffi;
    use crate::network_runtime::accepted_transport::AcceptedSession;
    use crate::network_runtime::accepted_transport::ObservationReceipt;
    use crate::network_runtime::accepted_transport::Received;

    async fn next_request(peer: &mut AcceptedSession) -> io::Result<u64> {
        loop {
            match peer.try_receive()? {
                Some(Received::Request(sequence)) => return Ok(sequence),
                Some(_) => return Err(io::Error::other("unexpected fixture response")),
                None => tokio::task::yield_now().await,
            }
        }
    }

    async fn completed_journal_observation(
        runtime: &NetworkRuntimeResources,
        controller: &accepted_controller::Controller,
        peer: &mut AcceptedSession,
    ) -> io::Result<(u64, Vec<u8>)> {
        let thread = crate::types::DetTid::from_raw(93);
        let who = crate::network_replay::NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        let read = async {
            runtime
                .shared
                .fd_journal
                .lock()
                .await
                .through(controller, who, 1)
                .await
        };
        let reply = async {
            let sequence = next_request(peer).await?;
            let (request, rights, _) = peer.retained_request(sequence)?;
            if request.owner != Some(who)
                || !rights.is_empty()
                || !matches!(
                    serde_json::from_slice::<Request>(&request.body)?,
                    Request::AwaitFdEvent {
                        sequence: 1,
                        acknowledged: None
                    }
                )
            {
                return Err(io::Error::other(
                    "journal fixture received a different request",
                ));
            }
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
            })?;
            peer.begin_observation(sequence)?;
            peer.finish_observation(sequence, body.clone())?;
            peer.acknowledge_command_completion(sequence, |_, raw| {
                if raw != body {
                    return Err(io::Error::other("journal fixture ACK changed bytes"));
                }
                Ok(b"exact fixture provider ACK".to_vec())
            })?;
            if !peer.try_reply(sequence)? {
                return Err(io::Error::other("journal fixture reply did not send"));
            }
            Ok((sequence, body))
        };
        let ((), observed) = tokio::try_join!(read, reply)?;
        Ok(observed)
    }

    fn exact_retirement(
        peer: &AcceptedSession,
        sequence: u64,
        observed: &(u64, Vec<u8>),
    ) -> io::Result<ObservationReceipt> {
        let (request, rights, _) = peer.retained_request(sequence)?;
        let Request::RetireFdObservation { receipt } = serde_json::from_slice(&request.body)?
        else {
            return Err(io::Error::other("final journal operation changed kind"));
        };
        let (original, _, _) = peer.retained_request(observed.0)?;
        if request.owner != original.owner
            || !rights.is_empty()
            || receipt.sequence != observed.0
            || receipt.body != observed.1
            || !receipt.fd_journal
        {
            return Err(io::Error::other(
                "final journal receipt changed identity or bytes",
            ));
        }
        Ok(receipt)
    }

    fn acknowledge_retirement(
        peer: &mut AcceptedSession,
        sequence: u64,
        receipt: &ObservationReceipt,
    ) -> io::Result<()> {
        peer.retire_incoming_observation(receipt)?;
        peer.dispatch(sequence, |_, _| Ok(serde_json::to_vec(&Reply::Retired)?))?;
        if !peer.try_reply(sequence)? {
            return Err(io::Error::other(
                "final retirement fixture reply did not send",
            ));
        }
        Ok(())
    }

    async fn check_failed_backend_retirement(withhold_ack: bool) {
        let (mut owner, runtime, controller, endpoint) = started_accepted_driver_fixture();
        let mut peer = AcceptedSession::new(endpoint, [93; 16]).unwrap();
        let setup = tokio::time::timeout(
            Duration::from_secs(1),
            completed_journal_observation(&runtime, &controller, &mut peer),
        )
        .await;
        if !matches!(&setup, Ok(Ok(_))) {
            owner.stop_collection_fixture(Instant::now() + Duration::from_secs(2));
        }
        let observed = setup
            .expect("bounded journal setup")
            .expect("completed journal observation");
        let original = Instant::now() + Duration::from_secs(1);
        *owner.shared.transport_terminal_deadline.lock().unwrap() = Some(original);
        let (outcome, retirement) = {
            let finishing = unsafe { owner.finish_native_controller_tasks() };
            tokio::pin!(finishing);
            tokio::select! {
                biased;
                result = &mut finishing => (result, Ok(None)),
                request = next_request(&mut peer) => {
                    let retirement = request.and_then(|sequence| {
                        let receipt = exact_retirement(&peer, sequence, &observed)?;
                        if !withhold_ack {
                            acknowledge_retirement(&mut peer, sequence, &receipt)?;
                        }
                        Ok(Some((sequence, receipt)))
                    });
                    (finishing.await, retirement)
                }
            }
        };
        let joined_at_return = accepted_driver_joined(&owner);
        let deadline_at_return = *owner.shared.transport_terminal_deadline.lock().unwrap();
        let quiescent_at_return = controller.quiescent();
        let mut retry = None;
        let mut deadline_after_retry = None;
        let mut joined_after_retry = None;
        let mut late_ack = None;
        if withhold_ack {
            retry = Some(unsafe { owner.finish_native_controller_tasks().await });
            deadline_after_retry = *owner.shared.transport_terminal_deadline.lock().unwrap();
            joined_after_retry = Some(accepted_driver_joined(&owner));
            if let Ok(Some((sequence, receipt))) = &retirement {
                late_ack = Some(
                    match acknowledge_retirement(&mut peer, *sequence, receipt) {
                        Ok(()) => tokio::time::timeout(
                            Duration::from_secs(1),
                            controller.response(*sequence),
                        )
                        .await
                        .map_err(|_| io::Error::other("fixture late ACK was not retained"))
                        .and_then(|result| result),
                        Err(error) => Err(error),
                    },
                );
            }
        }
        // Even the uncorrected implementation exits this select immediately;
        // it cannot leave a fixture thread alive while failing the new oracle.
        owner.stop_collection_fixture(Instant::now() + Duration::from_secs(2));
        let extra_request = peer.try_receive();
        let original_retired = peer.retained_request(observed.0).is_err();
        let history_next = runtime.shared.fd_journal.lock().await.history().next();

        assert!(
            retirement.as_ref().is_ok_and(|request| request.is_some()),
            "failed backend must send the exact final journal retirement: {retirement:?}"
        );
        assert_eq!(deadline_at_return, Some(original));
        assert_eq!(
            history_next.unwrap(),
            2,
            "original journal bytes remain run-owned"
        );
        assert!(
            extra_request.unwrap().is_none(),
            "no replacement retirement request"
        );
        assert!(
            original_retired,
            "the original peer observation was actually retired"
        );
        assert!(accepted_driver_joined(&owner));
        if withhold_ack {
            assert_eq!(
                outcome.unwrap_err().to_string(),
                "accepted terminal transport deadline"
            );
            assert!(
                !joined_at_return,
                "a withheld ACK must retain the original driver"
            );
            assert!(!quiescent_at_return.unwrap());
            assert!(
                retry.unwrap().is_err(),
                "retry cannot renew the terminal budget"
            );
            assert_eq!(deadline_after_retry, Some(original));
            assert_eq!(joined_after_retry, Some(false));
            assert!(matches!(late_ack.unwrap().unwrap(), Reply::Retired));
        } else {
            outcome.unwrap();
            assert!(joined_at_return, "successful cleanup must join its driver");
            assert!(quiescent_at_return.unwrap());
        }
    }

    #[tokio::test]
    async fn failed_backend_sends_final_journal_ack_before_driver_join() {
        check_failed_backend_retirement(false).await;
    }

    #[tokio::test]
    async fn withheld_final_journal_ack_retains_driver_and_original_deadline() {
        check_failed_backend_retirement(true).await;
    }
}
