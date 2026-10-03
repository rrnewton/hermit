//! Bounded V4 poll: raw scans and a finite, sole-root retained-OFD wait.
use detcore_model::network_trace::tcp_poll_row_mask;

use super::*;

fn single_scan_result(rows: &[libc::pollfd], zero_timeout: bool) -> Result<i64, Error> {
    let ready = rows.iter().filter(|row| row.revents != 0).count() as i64;
    if ready == 0 && !zero_timeout {
        return Err(engine_error("V4 poll blocking wait is not implemented"));
    }
    Ok(ready)
}

fn select_tcp(
    canonical: &mut Option<(i32, OpenFileId)>,
    fd: i32,
    file: OpenFileId,
) -> Result<(), Error> {
    match *canonical {
        Some((_, previous)) if previous != file => Err(engine_error(
            "V4 poll supports one TCP OFD plus local pairs",
        )),
        None => {
            *canonical = Some((fd, file));
            Ok(())
        }
        _ => Ok(()),
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3464): actual raw readiness, never expected output.
    pub(super) async fn native_poll_single_scan<G: Guest<Self>>(
        &self,
        guest: &mut G,
        state: NetworkPollState,
        policy: NetworkPolicy,
    ) -> Result<i64, Error> {
        let address = state.poll_address()?;
        let mut output = read_pollfds(guest, address, state.count)?;
        let mut local = Vec::with_capacity(output.len());
        for row in &output {
            let is_local = row.fd >= 0
                && guest
                    .thread_state()
                    .with_detfd(row.fd, |fd| fd.is_local_socket_pair())
                    .unwrap_or(false);
            local.push(is_local);
        }
        let start = thread_observe_time(guest).await;
        let deadline = state.timeout.map(|duration| start + duration);
        // Local endpoints are probed identically in Record and Replay, before
        // the TCP Call captures the single foreground publication interval.
        for (row, is_local) in output.iter_mut().zip(&local) {
            row.revents = 0;
            if *is_local {
                *row = self.probe_shadow_local_pair(guest, *row).await?;
            }
        }
        // The actual local probes are over before TCP admission. Retain exact
        // bindings, not only a copied local marker, and never inject a guest
        // probe during the following host-only wait.
        let local_bindings: Vec<_> = output
            .iter()
            .zip(&local)
            .filter(|(_, local)| **local)
            .map(|(row, _)| {
                guest
                    .thread_state()
                    .file_metadata
                    .lock()
                    .unwrap()
                    .descriptor_binding(row.fd)
                    .map(|binding| (row.fd, binding))
            })
            .collect::<Result<_, _>>()?;
        let mut classified = std::collections::BTreeMap::new();
        let mut tcp = None;
        for (row, is_local) in output.iter().zip(&local) {
            if *is_local || row.fd < 0 || classified.contains_key(&row.fd) {
                continue;
            }
            let file = self.classify_native_poll_fd(guest, row.fd).await?;
            if let Some(file) = file {
                select_tcp(&mut tcp, row.fd, file)?;
            }
            classified.insert(row.fd, file);
        }
        let other_ready = output.iter().zip(&local).any(|(row, local)| {
            (*local && row.revents != 0)
                || (!*local && row.fd >= 0 && classified[&row.fd].is_none())
        });
        let events = output
            .iter()
            .zip(&local)
            .filter(|(row, local)| !**local && row.fd >= 0 && classified[&row.fd].is_some())
            .fold(0, |events, (row, _)| events | row.events);
        let wait = if other_ready || state.timeout == Some(Duration::ZERO) {
            None
        } else {
            deadline.map(|deadline| (events, deadline))
        };
        let (raw, waited) = match tcp {
            Some((fd, file)) => {
                let (raw, waited) = self
                    .native_poll_snapshot(guest, fd, file, policy, wait, &local_bindings)
                    .await?;
                (Some(raw), waited)
            }
            None => (None, false),
        };
        {
            for (row, is_local) in output.iter_mut().zip(&local) {
                if !*is_local && row.fd >= 0 {
                    row.revents = match classified[&row.fd] {
                        None => libc::POLLNVAL,
                        Some(_) => {
                            tcp_poll_row_mask(raw.expect("admitted TCP observation"), row.events)
                        }
                    };
                }
            }
        }
        // Call/pin closure is complete before any guest result-buffer write.
        // A zero snapshot is not permission to report a future timeout.
        let count = single_scan_result(&output, waited || state.timeout == Some(Duration::ZERO))?;
        write_pollfds(guest, address, &output)?;
        let now = thread_observe_time(guest).await;
        write_remaining_timeout(guest, state.remaining_address, deadline, now)?;
        Ok(count)
    }

    async fn classify_native_poll_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
    ) -> Result<Option<OpenFileId>, Error> {
        let read = self.begin_network_fd_read(guest, fd).await?;
        let operation = async {
            let observed = guest
                .thread_state()
                .file_metadata
                .lock()
                .unwrap()
                .observe_fd_read(&read)?;
            if observed.binding.is_none() && observed.socket.is_none() {
                return Ok(None);
            }
            let file = observed
                .socket
                .ok_or_else(|| engine_error("V4 poll mixed unsupported descriptor"))?;
            if observed.binding != read.binding
                || read.binding.is_none_or(|binding| binding.open_file != file)
            {
                return Err(engine_error("V4 poll changed its admitted binding"));
            }
            let socket = self
                .shadow_socket_state(guest, file)
                .await?
                .ok_or_else(|| engine_error("V4 poll requires an enrolled TCP profile"))?;
            if socket.key.transport != NetworkTransportV2::Tcp
                || socket.key.socket_type != libc::SOCK_STREAM
                || socket.key.protocol != libc::IPPROTO_TCP
            {
                return Err(engine_error("V4 poll requires its TCP socket profile"));
            }
            Ok(Some(file))
        }
        .await;
        let cleanup = self
            .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
            .await;
        finish_shadow_operation(operation, cleanup)
    }

    async fn native_poll_snapshot<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        expected: OpenFileId,
        policy: NetworkPolicy,
        wait: Option<(i16, LogicalTime)>,
        locals: &[(i32, crate::types::FdSlotBinding)],
    ) -> Result<(i16, bool), Error> {
        if !self.network_fd_tracking_active(guest)
            || (policy == NetworkPolicy::Record && !guest.config().backend_supports_host_socket_pin)
        {
            return Err(engine_error(
                "V4 poll lacks authenticated backend FD/pin support",
            ));
        }
        let read = self.begin_network_fd_read(guest, fd).await?;
        let observed = guest
            .thread_state()
            .file_metadata
            .lock()
            .unwrap()
            .observe_fd_read(&read);
        let validated = (|| {
            let observed = observed?;
            match (observed.binding, observed.socket, observed.nonblocking) {
                (Some(binding), Some(socket), Some(nonblocking))
                    if Some(binding) == read.binding
                        && binding.open_file == socket
                        && socket == expected =>
                {
                    Ok(nonblocking)
                }
                _ => Err(engine_error(
                    "V4 poll descriptor is not its admitted TCP socket",
                )),
            }
        })();
        let nonblocking = match validated {
            Ok(nonblocking) => nonblocking,
            Err(error) => {
                let cleanup = self
                    .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
                    .await;
                return finish_shadow_operation(Err(error), cleanup);
            }
        };
        let Some(global) = guest.local_global_state() else {
            let cleanup = self
                .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
                .await;
            return finish_shadow_operation(
                Err(engine_error("V4 poll lacks local global state")),
                cleanup,
            );
        };
        let admitted = match policy {
            NetworkPolicy::Record => {
                global
                    .begin_native_poll_call(guest.tid(), guest.thread_state(), read)
                    .await
            }
            NetworkPolicy::Replay => {
                global.begin_replay_poll_call(guest.tid(), guest.thread_state(), read)
            }
            _ => unreachable!("network-only poll dispatch"),
        };
        let call = match admitted {
            Ok(call) => call,
            Err(failure) => {
                let failure = global.cleanup_receive_admission_failure(failure).await;
                let primary = engine_rpc_error(failure.primary().clone());
                let cleanup = failure
                    .cleanup_diagnostic()
                    .map_or(Ok(()), |e| Err(engine_rpc_error(e.clone())));
                return finish_shadow_operation(Err(primary), cleanup);
            }
        };
        let pin = NetworkHostSocketPin {
            call: call.id,
            native: call.physical_pin_required,
            nonblocking,
        };
        let mut waited = false;
        let mut record_lease = None;
        let operation = async {
            let (lease, initial) = if policy == NetworkPolicy::Record {
                let probe = match network_request(
                    guest,
                    NetworkRequest::BeginShadowProbe { call: call.id },
                )
                .await
                .map_err(engine_rpc_error)?
                {
                    NetworkReply::ShadowProbe(probe) => probe,
                    other => {
                        return Err(engine_error(format!(
                            "unexpected native poll probe {other:?}"
                        )));
                    }
                };
                record_lease = Some(probe.lease);
                let observed = self
                    .native_stream_effect(
                        guest,
                        probe.lease,
                        NetworkStreamPhysicalEffect::PollState,
                    )
                    .await?;
                let NetworkStreamPhysicalResult::PollState { revents } = observed.confirmation
                else {
                    return Err(engine_error(
                        "initial native poll did not complete a raw scan",
                    ));
                };
                (Some(probe.lease), revents)
            } else {
                (
                    None,
                    guest
                        .local_global_state()
                        .ok_or_else(|| engine_error("V4 poll lost local global state"))?
                        .finish_foreground_poll(guest.tid(), guest.thread_state(), call.id, None)
                        .map_err(engine_rpc_error)?,
                )
            };
            let global = guest
                .local_global_state()
                .ok_or_else(|| engine_error("V4 poll lost local global state"))?;
            if let Some(wait) = wait
                && tcp_poll_row_mask(initial, wait.0) == 0
            {
                let raw = global
                    .wait_foreground_poll(
                        guest.tid(),
                        guest.thread_state(),
                        call.id,
                        lease,
                        wait,
                        locals,
                    )
                    .await
                    .map_err(engine_rpc_error)?;
                waited = true;
                Ok(i64::from(raw))
            } else if lease.is_some() {
                global
                    .finish_foreground_poll(guest.tid(), guest.thread_state(), call.id, lease)
                    .map(i64::from)
                    .map_err(engine_rpc_error)
            } else {
                Ok(i64::from(initial))
            }
        }
        .await;
        let operation = match (operation, record_lease) {
            (Err(primary), Some(lease)) => {
                let cleanup = guest
                    .local_global_state()
                    .ok_or_else(|| engine_error("V4 poll cleanup lost local global state"))
                    .and_then(|global| {
                        global
                            .abort_foreground_poll(
                                crate::network_replay::NetworkStreamOwner {
                                    thread: guest.thread_state().dettid,
                                    mm: guest.thread_state().mm_id,
                                },
                                call.id,
                                lease,
                            )
                            .map_err(engine_rpc_error)
                    });
                finish_shadow_operation(Err(primary), cleanup)
            }
            (operation, _) => operation,
        };
        self.finish_host_stream_call(guest, pin, operation)
            .await
            .map(|raw| (raw as i16, waited))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_poll_single_scan_keeps_duplicate_masks_and_refuses_unfinished_wait() {
        let raw = libc::POLLIN
            | libc::POLLRDNORM
            | libc::POLLOUT
            | libc::POLLWRNORM
            | libc::POLLPRI
            | libc::POLLRDHUP;
        let events = [
            libc::POLLIN,
            libc::POLLRDNORM,
            libc::POLLPRI,
            libc::POLLRDHUP,
            libc::POLLWRNORM,
            0,
        ];
        let rows: Vec<_> = events
            .into_iter()
            .map(|events| libc::pollfd {
                fd: 5,
                events,
                revents: tcp_poll_row_mask(raw, events),
            })
            .collect();
        assert_eq!(rows.iter().map(|r| r.revents).collect::<Vec<_>>(), events);
        assert_eq!(single_scan_result(&rows, false).unwrap(), 5);
        let empty = [libc::pollfd {
            fd: 5,
            events: libc::POLLIN,
            revents: 0,
        }];
        assert_eq!(single_scan_result(&empty, true).unwrap(), 0);
        assert!(single_scan_result(&empty, false).is_err());
        assert!(single_scan_result(&[], false).is_err());
        let terminal = [libc::pollfd {
            fd: 5,
            events: 0,
            revents: libc::POLLHUP | libc::POLLERR,
        }];
        assert_eq!(single_scan_result(&terminal, false).unwrap(), 1);
        let owner = crate::types::DetTid::from_raw(61);
        let file = OpenFileId::new_socket(owner, 0);
        let mut canonical = None;
        select_tcp(&mut canonical, 5, file).unwrap();
        select_tcp(&mut canonical, 5, file).unwrap();
        select_tcp(&mut canonical, 8, file).unwrap();
        assert_eq!(
            canonical,
            Some((5, file)),
            "dup aliases keep the same physical scan"
        );
        assert!(select_tcp(&mut canonical, 9, OpenFileId::new_socket(owner, 1)).is_err());
        assert_eq!(canonical, Some((5, file)));
    }
}
