//! Poll input reserves worker exclusion before backend whole-cohort capture.
use super::*;

impl NetworkRuntimeResources {
    pub(crate) fn prepare_shared_poll_input(
        &self,
        prefix: &JoinedSharedPrefix,
        lineage: &SharedForegroundLineage<'_>,
        engine: &mut NetworkReplayEngine,
    ) -> std::io::Result<NativeSourceInterval> {
        if prefix.selected.is_some()
            || !Arc::ptr_eq(lineage.root(), prefix.root())
            || prefix.census.rows.iter().any(|row| {
                !lineage
                    .members()
                    .any(|member| Arc::ptr_eq(member, &row.root))
            })
        {
            return Err(std::io::Error::other(
                "Poll input changed original complete lineage",
            ));
        }
        self.with_shared_prefix_locks(prefix, engine, |_, owned, calls| {
            self.shared.reserve_shared_poll_input_interval(
                owned,
                prefix.root().clone(),
                prefix.prefix.generation,
                prefix.census.clone(),
                calls,
            )
        })
    }
}
