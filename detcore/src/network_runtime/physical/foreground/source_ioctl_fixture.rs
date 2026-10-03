//! Controlled dispatcher/census premises before actual initial-root issuance.
//! No native provider observation or actual ioctl is represented by this fixture.
use reverie::Tool;

use super::*;
use crate::network_runtime::accepted_provider_ffi as ffi;
use crate::network_runtime::original_installation::FileIdentity;

impl ForegroundRoot {
    pub(crate) fn controlled_source_ioctl_runtime(
        raw: i32,
        dispatch: u64,
    ) -> ControlledRuntimeFixture {
        let thread = DetTid::from_raw(raw);
        let before = MmId::initial(thread);
        let owner = NetworkStreamOwner {
            thread,
            mm: before.for_exec(thread),
        };
        let files = crate::types::FilesIdAllocator::default().allocate_exec(thread);
        let exec = ExecFilesReceipt {
            caller: thread,
            process: thread,
            mm: before,
            old_files: FilesId::initial(thread),
            new_files: files,
        };
        let mut tasks = CustodyTasks::default();
        tasks.register(owner, raw, raw, || Ok(99)).unwrap();
        tasks.bind_initial_exec(owner, exec).unwrap();
        tasks.begin_initial(owner).unwrap();
        let (mut association, _) = super::super::initial_root_fixture(owner, raw);
        association.files = files;
        association.enrollment.references = 1;
        association.enrollment.mode = 1;
        association.enrollment.phases = 7;
        association.enrollment.slots = 64;
        association.enrollment.files = 2;
        association.enrollment.end = 4;
        let regular = dispatch == ffi::SOURCE_IOCTL_DISPATCH_BTRFS;
        let mode = if regular {
            libc::S_IFREG
        } else {
            libc::S_IFCHR
        } | 0o600;
        let (major, minor) = if regular { (0, 0) } else { (1, 3) };
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
                    mode,
                    status_flags: libc::O_WRONLY as u32,
                    device_major: major,
                    device_minor: minor,
                    source_ioctl_dispatch: dispatch,
                    ..ffi::FdEvent::default()
                }
                .into()
            })
            .collect();
        let stats: Vec<_> = [1]
            .into_iter()
            .map(|fd| {
                let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                stat.st_mode = mode;
                stat.st_rdev = libc::makedev(major, minor);
                InitialFileStat {
                    fd,
                    physical_file: 37,
                    stat: stat.into(),
                }
            })
            .collect();
        let tid = reverie::Tid::from_raw(raw);
        let tool: crate::Detcore = crate::Detcore::new(tid, &crate::config::Config::default());
        let state = tool.init_thread_state(tid, None);
        let mut metadata = state.file_metadata.lock().unwrap().clone();
        metadata.files_id = files;
        let (candidate, claim, fence) = metadata
            .prepare_initial_census(owner, association.view(), stats.clone())
            .unwrap();
        association.check_claim(&claim).unwrap();
        metadata.commit_initial_census(candidate, fence).unwrap();
        metadata
            .bind_native_population(
                &FileIdentity::from_initial_census(&association, &claim).unwrap(),
            )
            .unwrap();
        let enrollment = tasks.enrollment(owner).unwrap();
        enrollment.association = Some(association);
        enrollment.settled = true;
        let captured = stats
            .into_iter()
            .map(|stat| InitialFileCapture {
                fd: stat.fd,
                physical_file: stat.physical_file,
                capture: crate::network_runtime::accepted_provider::CallStatus {
                    operation: "controlled initial ioctl file".into(),
                    returned: 71,
                    errno: None,
                },
                metadata: Ok((stat.stat, libc::O_WRONLY, None)),
                release: Some(crate::network_runtime::accepted_provider::CallStatus {
                    operation: "controlled auxiliary close".into(),
                    returned: 0,
                    errno: None,
                }),
            })
            .collect();
        tasks
            .observe_initial_metadata(owner, |_, _, _| Ok(captured))
            .unwrap();
        tasks
            .admit_semantics(owner, claim.clone(), |_, _, _, _| Ok(()))
            .unwrap();
        runtime_from_controlled_tasks(
            raw,
            (
                tasks,
                owner,
                Arc::new(Mutex::new(metadata)),
                Arc::new(Mutex::new(MemoryMetadata::new())),
                claim,
            ),
        )
    }
}
