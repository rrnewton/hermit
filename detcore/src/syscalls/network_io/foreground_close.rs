//! Keep a proven finite Close on the original Normal turn. Unknown provenance
//! returns to the unchanged external Close before any original Call is staged.
use super::*;

impl<T: RecordOrReplay> Detcore<T> {
    #[cfg(test)]
    pub(crate) async fn controlled_foreground_close<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Close,
    ) -> Result<Option<i64>, Error> {
        self.try_network_foreground_close(guest, call).await
    }

    pub(super) async fn try_network_foreground_close<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Close,
    ) -> Result<Option<i64>, Error> {
        if !guest
            .local_global_state()
            .is_some_and(|global| global.foreground_close_policy_enabled())
        {
            return Ok(None);
        }
        let read = self.begin_network_fd_read(guest, call.fd()).await?;
        let mut transferred = false;
        let result = async {
            if !guest
                .local_global_state()
                .unwrap()
                .foreground_close_candidate(guest.tid(), guest.thread_state(), &read)
                .map_err(engine_rpc_error)?
            {
                return Ok(None);
            }
            self.check_original_call_staging(guest, crate::OriginalFileExecution::Native)?;
            let arguments = self.stage_original_call_local(
                guest,
                call.into(),
                crate::network_replay::original_connect::Kind::Close,
                (call.fd(), 0, 0),
            )?;
            let (_, args) = Syscall::from(call).into_parts();
            let raw = [
                args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5,
            ];
            let prepared = guest
                .local_global_state()
                .unwrap()
                .begin_foreground_original_close(
                    guest.tid(),
                    guest.thread_state(),
                    read.clone(),
                    arguments,
                    raw,
                )
                .await
                .map_err(engine_rpc_error)?;
            let admission = prepared.origin.admission().clone();
            transferred = true;
            let local = guest.thread_state_mut().original_connect.as_mut().unwrap();
            local.arguments = admission.arguments.clone();
            local.admission = Some(admission.clone());
            guest
                .local_global_state()
                .unwrap()
                .prepare_foreground_original_close(prepared)
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
            let returned = guest.inject(call).await.map_err(Error::from);
            let (_, outcome, returned, _) = self
                .observe_original_call_result(guest, admission.clone(), returned)
                .await?;
            if outcome.pin.is_some() || outcome.address.is_some() {
                return Err(engine_error(
                    "finite Close completion contains duplicate pin or copy",
                ));
            }
            let retirement = self
                .shadow_ack(
                    guest,
                    NetworkRequest::NativeRetireOriginalConnect { admission },
                )
                .await;
            if retirement.is_ok() {
                guest.thread_state_mut().original_connect = None;
            }
            finish_shadow_operation(returned, retirement).map(Some)
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
}
