//! Per-runtime test premise for original installation and copy5 geometry only.
//! Payloads and effects still come from actual TCP and the existing workers.
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::Mutex;

use super::super::NetworkRuntimeResources;

type CaptureGate = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

#[derive(Debug)]
pub(crate) struct ControlledRecordReceiveFixture {
    state: Mutex<State>,
}
#[derive(Debug)]
struct State {
    pin: Option<OwnedFd>,
    order: Vec<&'static str>,
    gate: Option<CaptureGate>,
}
impl ControlledRecordReceiveFixture {
    pub(crate) fn order(&self) -> Vec<&'static str> {
        self.state.lock().unwrap().order.clone()
    }
    pub(crate) fn pause_capture(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let mut state = self.state.lock().unwrap();
        assert!(state.order.is_empty() && state.gate.is_none());
        let (started, observed) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        state.gate = Some((started, released));
        (observed, release)
    }
    pub(super) fn take_capture(
        &self,
    ) -> std::io::Result<Box<dyn FnOnce() -> std::io::Result<OwnedFd> + Send>> {
        let mut state = self.state.lock().unwrap();
        if !state.order.is_empty() {
            return Err(std::io::Error::other("controlled capture was already used"));
        }
        let pin = state
            .pin
            .take()
            .ok_or_else(|| std::io::Error::other("controlled capture lost pin"))?;
        let gate = state.gate.take();
        state.order.push("capture");
        Ok(Box::new(move || {
            if let Some((started, released)) = gate {
                started
                    .send(())
                    .map_err(|_| std::io::Error::other("test capture observer dropped"))?;
                released
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .map_err(std::io::Error::other)?;
            }
            Ok(pin)
        }))
    }
    pub(super) fn begin_peek(&self) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.order != ["capture"] {
            return Err(std::io::Error::other(
                "controlled Peek changed one-use order",
            ));
        }
        state.order.push("peek");
        Ok(())
    }
    pub(super) fn begin_drain(&self) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.order != ["capture", "peek"] {
            return Err(std::io::Error::other(
                "controlled Drain changed one-use order",
            ));
        }
        state.order.push("drain");
        Ok(())
    }
}
impl NetworkRuntimeResources {
    pub(crate) fn install_controlled_record_receive(
        &self,
        pin: OwnedFd,
    ) -> Arc<ControlledRecordReceiveFixture> {
        let fixture = Arc::new(ControlledRecordReceiveFixture {
            state: Mutex::new(State {
                pin: Some(pin),
                order: vec![],
                gate: None,
            }),
        });
        let mut slot = self.shared.record_receive_fixture.lock().unwrap();
        assert!(
            slot.is_none(),
            "fixture cannot replace another runtime premise"
        );
        *slot = Some(fixture.clone());
        fixture
    }
}
