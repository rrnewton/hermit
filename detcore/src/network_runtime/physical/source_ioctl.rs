//! Dispatcher evidence remains private in the authenticated initial census.
use reverie::OriginalIoctlDispatch;

use super::InitialTableAssociation;
use crate::network_runtime::accepted_provider_ffi::SOURCE_IOCTL_DISPATCH_BTRFS;
use crate::network_runtime::accepted_provider_ffi::SOURCE_IOCTL_DISPATCH_NULL;
use crate::network_runtime::original_installation::FileIdentity;

impl InitialTableAssociation {
    /// This closed enum alone is not a certificate. The caller must join the
    /// current native binding, actual metadata and original stopped ENTRY.
    pub(crate) fn source_ioctl_dispatch(
        &self,
        identity: FileIdentity,
    ) -> Option<OriginalIoctlDispatch> {
        self.validate_root_identity().ok()?;
        let mut aliases = self
            .slots
            .iter()
            .filter(|row| identity.matches(self.provider, row.file));
        let first = aliases.next()?;
        let profile = |row: &super::FdEvent| {
            (
                row.source_ioctl_dispatch,
                row.mode,
                row.status_flags,
                row.device_major,
                row.device_minor,
            )
        };
        if first.kind != 21
            || first.complete != 1
            || first.table != self.enrollment.table
            || aliases.any(|row| {
                row.kind != 21
                    || row.complete != 1
                    || row.table != first.table
                    || profile(row) != profile(first)
            })
        {
            return None;
        }
        match first.source_ioctl_dispatch {
            SOURCE_IOCTL_DISPATCH_NULL
                if first.mode & libc::S_IFMT == libc::S_IFCHR
                    && first.device_major == 1
                    && first.device_minor == 3 =>
            {
                Some(OriginalIoctlDispatch::NullFileOperations)
            }
            SOURCE_IOCTL_DISPATCH_BTRFS
                if first.mode & libc::S_IFMT == libc::S_IFREG
                    && first.device_major == 0
                    && first.device_minor == 0 =>
            {
                Some(OriginalIoctlDispatch::BtrfsRegularFileOperations)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_replay::NetworkStreamOwner;
    use crate::network_runtime::accepted_provider_ffi as ffi;
    use crate::types::DetTid;
    use crate::types::MmId;

    fn census(dispatch: u64) -> InitialTableAssociation {
        let thread = DetTid::from_raw(61);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let (mut association, _) = super::super::initial_root_fixture(owner, 61);
        association.enrollment.references = 1;
        association.enrollment.mode = 1;
        association.enrollment.phases = 7;
        association.enrollment.slots = 64;
        association.enrollment.files = 2;
        association.enrollment.end = 4;
        let regular = dispatch == SOURCE_IOCTL_DISPATCH_BTRFS;
        association.slots = [1, 3]
            .into_iter()
            .enumerate()
            .map(|(index, fd)| {
                ffi::FdEvent {
                    sequence: index as u64 + 2,
                    kind: 21,
                    task: association.enrollment.task,
                    task_start: association.enrollment.task_start,
                    table: association.enrollment.table,
                    file: 37,
                    dependency: 1,
                    accept_command: association.enrollment.command,
                    fd,
                    complete: 1,
                    mode: if regular {
                        libc::S_IFREG
                    } else {
                        libc::S_IFCHR
                    },
                    device_major: if regular { 0 } else { 1 },
                    device_minor: if regular { 0 } else { 3 },
                    source_ioctl_dispatch: dispatch,
                    ..ffi::FdEvent::default()
                }
                .into()
            })
            .collect();
        association
    }

    #[test]
    fn source_ioctl_dispatch_requires_retained_provider_file_and_all_initial_aliases() {
        for dispatch in [SOURCE_IOCTL_DISPATCH_NULL, SOURCE_IOCTL_DISPATCH_BTRFS] {
            let association = census(dispatch);
            let identity = FileIdentity::controlled_fixture(association.provider, 37);
            let result = association.source_ioctl_dispatch(identity).unwrap();
            assert!(matches!(
                (dispatch, result),
                (
                    SOURCE_IOCTL_DISPATCH_NULL,
                    OriginalIoctlDispatch::NullFileOperations
                ) | (
                    SOURCE_IOCTL_DISPATCH_BTRFS,
                    OriginalIoctlDispatch::BtrfsRegularFileOperations
                )
            ));
            for wrong in [
                FileIdentity::controlled_fixture(9, 37),
                FileIdentity::controlled_fixture(3, 38),
            ] {
                assert!(association.source_ioctl_dispatch(wrong).is_none());
            }
            for mutation in 0..11 {
                let mut changed = association.clone();
                match mutation {
                    0 => changed.slots.clear(),
                    1 => changed.slots[1].source_ioctl_dispatch = 0,
                    2 => changed.slots[1].source_ioctl_dispatch = 3,
                    3 => changed.slots[1].mode = libc::S_IFIFO,
                    4 => changed.slots[1].device_major += 1,
                    5 => changed.slots[1].device_minor += 1,
                    6 => changed.slots[1].status_flags += 1,
                    7 => changed.slots[1].complete = 0,
                    8 => changed.slots[1].kind = 2,
                    9 => changed.slots[1].table += 1,
                    10 => changed.enrollment.task_start = 0,
                    _ => unreachable!(),
                }
                assert!(
                    changed.source_ioctl_dispatch(identity).is_none(),
                    "mutation {mutation}"
                );
            }
            // Even mutually agreeing aliases cannot invent a supported profile.
            for mutation in 0..5 {
                let mut changed = association.clone();
                for row in &mut changed.slots {
                    match mutation {
                        0 => row.source_ioctl_dispatch = 0,
                        1 => row.source_ioctl_dispatch = 3,
                        2 => row.mode = libc::S_IFIFO,
                        3 => row.device_major += 1,
                        4 => row.device_minor += 1,
                        _ => unreachable!(),
                    }
                }
                assert!(
                    changed.source_ioctl_dispatch(identity).is_none(),
                    "profile {mutation}"
                );
            }
        }
    }
}
