//! Blocking shared Record sends use the original syscall and its actual accepted
//! skb prefix. No speculative user read or external scheduler handoff occurs.
use super::*;

impl<T: RecordOrReplay> Detcore<T> {
    pub(super) async fn network_shared_original_sendto<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Sendto,
    ) -> Result<i64, Error> {
        if !original_sendto_shape(call)
            || !(1..=512).contains(&call.size())
            || guest
                .thread_state()
                .with_detfd(call.fd(), |fd| fd.is_nonblocking())?
        {
            return Err(engine_error(
                "shared original Sendto requires bounded blocking MSG_NOSIGNAL shape",
            ));
        }
        let read = self.begin_network_fd_read(guest, call.fd()).await?;
        let mut transferred = false;
        let result = async {
            let arguments = self.stage_shared_original_send_from_read(guest, call, &read)?;
            let (_, args) = Syscall::from(call).into_parts();
            let raw = [
                args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5,
            ];
            // Only the dedicated genuine peer timers may be joined. This does
            // not run a peer continuation or replace this original Sendto frame.
            guest.join_followed_observation_timers(call.into()).await?;
            let prepared = guest
                .local_global_state()
                .unwrap()
                .begin_shared_original_send(
                    guest.tid(),
                    guest.thread_state(),
                    read.clone(),
                    arguments,
                    raw,
                )
                .await
                .map_err(engine_rpc_error)?;
            let origin = prepared.origin.clone();
            let admission = origin.admission().clone();
            transferred = true;
            let local = guest.thread_state_mut().original_connect.as_mut().unwrap();
            local.arguments = admission.arguments.clone();
            local.admission = Some(admission.clone());
            guest
                .local_global_state()
                .unwrap()
                .prepare_shared_original_send(prepared)
                .await
                .map_err(engine_rpc_error)?;
            self.shadow_ack(
                guest,
                NetworkRequest::NativeSubmitOriginalConnect {
                    admission: admission.clone(),
                },
            )
            .await?;
            self.mark_original_syscall_invoked(guest);
            // Backend consumes the all-stopped hold into exactly this original
            // sender invocation, retaining actual peer gates through restoration.
            let returned = guest.inject_original_sendto_with_stopped_peers(call).await;
            if matches!(&returned, Err(Error::Tool(_) | Error::Io(_))) {
                return returned;
            }
            let (_, _, returned, _) = self
                .observe_original_call_result(guest, admission.clone(), returned)
                .await?;
            // The Driver's close reply is separate from actual worker retirement.
            guest
                .local_global_state()
                .unwrap()
                .join_shared_send_close(&origin)
                .await
                .map_err(engine_rpc_error)?;
            guest
                .local_global_state()
                .unwrap()
                .publish_shared_original_send(guest.tid(), guest.thread_state(), &origin)
                .map_err(engine_rpc_error)?;
            self.shadow_ack(
                guest,
                NetworkRequest::NativeRetireOriginalConnect { admission },
            )
            .await?;
            guest.thread_state_mut().original_connect = None;
            returned
        }
        .await;
        if transferred {
            return result;
        }
        let released = self
            .shadow_ack(guest, NetworkRequest::FinishFdRead { admission: read })
            .await;
        finish_shadow_operation(result, released)
    }

    /// The exact retained reader already owns publication custody. Reacquiring
    /// it would conflict with that same lease; see
    /// https://github.com/rrnewton/hermit/issues/3613 and
    /// https://github.com/rrnewton/hermit/pull/3464.
    fn stage_shared_original_send_from_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Sendto,
        read: &crate::network_replay::NetworkFdReadAdmission,
    ) -> Result<crate::network_replay::original_connect::Arguments, Error> {
        // This existing Global borrower validates current shared Record mode,
        // Normal/lineage, owner, table prefix, binding and descriptor control.
        // It also derives the finite timeout from the same modeled socket.
        let timeout = guest
            .local_global_state()
            .ok_or_else(|| engine_error("shared send lacks local Global"))?
            .shared_send_timeout(guest.tid(), guest.thread_state(), read)
            .map_err(engine_rpc_error)?;
        if call.fd() != read.fd {
            return Err(engine_error(
                "shared Sendto changed its admitted descriptor",
            ));
        }
        self.check_original_call_staging(guest, crate::OriginalFileExecution::Native)?;
        let (_, args) = Syscall::from(call).into_parts();
        // No await or reader release separates validation from Local staging.
        self.stage_original_call_local(
            guest,
            call.into(),
            crate::network_replay::original_connect::Kind::BlockingSendto {
                timeout_ticks: timeout,
            },
            (call.fd(), args.arg1 as u64, call.flags() as i32),
        )
    }
}
