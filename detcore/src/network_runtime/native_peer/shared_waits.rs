//! Complete actual native Call census for one serialized shared attempt. A
//! retained lifetime pin is allowed only for its exact authenticated engine Call.
use super::*;
use crate::network_replay::shared_waits::SharedCallCensus;

impl Calls {
    pub(in crate::network_runtime) fn require_shared_quiescence(
        &self,
        census: &SharedCallCensus,
    ) -> io::Result<()> {
        let expected = census.rows.iter().filter(|row| row.native.is_some());
        if expected.clone().count() != self.calls.len() {
            return Err(io::Error::other(
                "shared census omitted or added a native Call",
            ));
        }
        for row in expected {
            let state = self
                .calls
                .get(&row.call)
                .ok_or_else(|| io::Error::other("shared census lost actual retained pin"))?;
            if state.id != row.call
                || state.owner != row.owner
                || state.identity != row.native
                || state.acquisition.is_err()
                || state.original.is_none()
                || state.publication.is_none()
                || state.invocation.is_some()
                || state.terminal.is_some()
                || state.releasing
                || state.release.is_some()
                || !state.leases.is_empty()
            {
                return Err(io::Error::other(
                    "shared census retains unretired native/helper/cursor debt",
                ));
            }
        }
        Ok(())
    }
}
