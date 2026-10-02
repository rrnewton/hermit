//! Explicit native-runner selector for the shared Openat publisher. The sealed
//! C fixture supplies live kernel receipts. This is not a scheduler/startup E2E.
use std::collections::BTreeSet;
use std::ffi::CString;
use std::ffi::c_void;
use std::os::fd::FromRawFd;
use std::os::fd::IntoRawFd;
use std::os::fd::OwnedFd;

use chrono::TimeZone;
use chrono::Utc;
use detcore_model::time::LogicalTime;

use super::super::accepted_provider as wire;
use super::super::accepted_provider_ffi as ffi;
use super::super::physical;
use super::*;
use crate::network_replay::NetworkFdMutationAdmission;
use crate::network_replay::NetworkReplayEngine;
use crate::network_replay::original_connect::Admission;
use crate::network_replay::original_connect::Arguments;
use crate::network_replay::original_connect::Kind;
use crate::network_replay::original_installation::OpenatEnrollment;
use crate::resources::ExternalOpId;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::MmId;

#[repr(C)]
struct NativeStat {
    fd: i32,
    reserved: i32,
    stat: libc::stat,
}
#[repr(C)]
struct Event {
    version: u64,
    size: u64,
    stage: u64,
    owner_mm: u64,
    registration: u64,
    call: u64,
    command: u64,
    through: u64,
    pid: i32,
    pin: i32,
    dfd: i32,
    flags: i32,
    address: u64,
    count: u64,
    result: *const ffi::CommandResult,
    enrollment: *const ffi::FdEnrollment,
    original: *const ffi::OriginalResult,
    status: *const ffi::FdStatus,
    rows: *const ffi::FdEvent,
    row_count: u64,
    stats: *const NativeStat,
    stat_count: u64,
    opened_stat: *const libc::stat,
}
#[derive(Default)]
struct Context {
    stages: Vec<u64>,
    failed: bool,
    tasks: Option<physical::CustodyTasks<OwnedFd>>,
    owner: Option<NetworkStreamOwner>,
    association: Option<physical::InitialTableAssociation>,
    metadata: Option<Arc<Mutex<FileMetadata>>>,
    engine: Option<NetworkReplayEngine>,
    admission: Option<Admission>,
    mutation: Option<NetworkFdMutationAdmission>,
    history: History,
    original: Option<wire::OriginalResult>,
    capture_pin: Option<OwnedFd>,
    receipt: Option<Installation>,
}
fn clone_pin(fd: i32) -> OwnedFd {
    let raw = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    assert!(
        raw >= 0,
        "clone exact retained PIDFD: {}",
        io::Error::last_os_error()
    );
    unsafe { OwnedFd::from_raw_fd(raw) }
}
impl Context {
    fn retain(&mut self, event: &Event) {
        assert!(event.row_count <= 128 && event.row_count == event.through);
        assert!(!event.rows.is_null() && !event.status.is_null());
        let status: wire::FdStatus = unsafe { *event.status }.into();
        assert_eq!(status.next_event, event.through);
        let rows = unsafe { std::slice::from_raw_parts(event.rows, event.row_count as usize) };
        let next = self.history.next().unwrap();
        let suffix: Vec<wire::FdEvent> = rows
            .iter()
            .filter(|row| row.sequence >= next)
            .map(|row| (*row).into())
            .collect();
        println!(
            "OPENAT_PUBLICATION_JOURNAL {}",
            serde_json::to_string(&(status.clone(), &suffix)).unwrap()
        );
        for row in suffix {
            self.history.retain(status.clone(), row).unwrap();
        }
        assert_eq!(self.history.next().unwrap(), event.through + 1);
    }
    fn step(&mut self, event: &mut Event) {
        assert_eq!(event.version, 1);
        assert_eq!(event.size as usize, std::mem::size_of::<Event>());
        if event.stage == 9 {
            // Always release real aliases on failure; this cannot clear failed.
            self.capture_pin.take();
            self.tasks.take();
            self.stages.push(9);
            return;
        }
        assert!(!self.failed);
        assert_eq!(event.stage as usize, self.stages.len());
        match event.stage {
            0 => {
                assert!(event.pid > 0 && event.pin >= 0);
                let thread = DetTid::from_raw(event.pid);
                let owner = NetworkStreamOwner {
                    thread,
                    mm: MmId::initial(thread),
                };
                let mut tasks = physical::CustodyTasks::default();
                tasks
                    .register(owner, event.pid, event.pid, || Ok(clone_pin(event.pin)))
                    .unwrap();
                event.registration = tasks.begin_initial(owner).unwrap();
                event.owner_mm = owner.mm.generation();
                self.owner = Some(owner);
                self.tasks = Some(tasks);
            }
            1 => {
                self.retain(event);
                let owner = self.owner.unwrap();
                let tasks = self.tasks.as_mut().unwrap();
                assert!(!event.result.is_null() && !event.enrollment.is_null());
                let raw = wire::TableEnrollmentEffect {
                    command: unsafe { *event.result }.into(),
                    enrollment: unsafe { *event.enrollment }.into(),
                };
                println!(
                    "OPENAT_PUBLICATION_ENROLLMENT {}",
                    serde_json::to_string(&raw).unwrap()
                );
                tasks.retain_preparation(owner, Ok(1)).unwrap();
                let ticket = tasks.prepared(owner, 1, event.command).unwrap();
                tasks.native_read(owner, ticket, true).unwrap();
                tasks.retain_collection(owner, Ok(2)).unwrap();
                tasks
                    .retain_raw(
                        owner,
                        Ok(wire::Observation {
                            status: wire::CallStatus {
                                operation: "ap_collect_table_enrollment".into(),
                                returned: 0,
                                errno: None,
                            },
                            raw,
                        }),
                    )
                    .unwrap();
                let association = self
                    .history
                    .enrollment(&tasks.collected_binding(owner, ticket, 2).unwrap())
                    .unwrap();
                tasks.complete(owner, association.clone()).unwrap();
                self.association = Some(association);
            }
            2 => {
                let owner = self.owner.unwrap();
                let association = self.association.as_ref().unwrap();
                let view = association.view();
                assert_eq!(view.descriptors.len(), 5);
                assert_eq!(event.stat_count, 5);
                assert!(!event.stats.is_null());
                let stats = unsafe { std::slice::from_raw_parts(event.stats, 5) };
                let mut seen = BTreeSet::new();
                let observed = view
                    .descriptors
                    .iter()
                    .filter(|row| seen.insert(row.physical_file))
                    .map(|row| {
                        let matches: Vec<_> =
                            stats.iter().filter(|stat| stat.fd == row.fd).collect();
                        assert_eq!(matches.len(), 1);
                        assert_eq!(matches[0].reserved, 0);
                        physical::InitialFileStat {
                            fd: row.fd,
                            physical_file: row.physical_file,
                            stat: matches[0].stat.into(),
                        }
                    })
                    .collect();
                let actual = crate::tool_local::ThreadState::new(
                    owner.thread,
                    &crate::Config::default(),
                    (),
                )
                .file_metadata;
                let mut metadata = actual.lock().unwrap();
                let (candidate, claim, fence) = metadata
                    .prepare_initial_census(owner, view, observed)
                    .unwrap();
                let mut engine = NetworkReplayEngine::record_shadow(
                    Utc.timestamp_opt(1_790_000_000, 0).unwrap(),
                );
                engine
                    .admit_initial_record_census(association, &claim, DetPid::from_raw(event.pid))
                    .unwrap();
                metadata.commit_initial_census(candidate, fence).unwrap();
                metadata
                    .bind_native_population(
                        &FileIdentity::from_initial_census(association, &claim).unwrap(),
                    )
                    .unwrap();
                drop(metadata);
                engine
                    .associate_fd_metadata(owner, &actual, &actual.lock().unwrap())
                    .unwrap();
                let arguments = Arguments {
                    kind: Kind::Openat,
                    operation: ExternalOpId::new(owner.thread, 1),
                    files: claim.view.files,
                    binding: None,
                    fd: event.dfd,
                    address: event.address,
                    length: event.flags,
                    original_count: event.count,
                };
                assert_eq!(arguments.fd, libc::AT_FDCWD);
                assert_eq!(arguments.length, libc::O_RDONLY | libc::O_CLOEXEC);
                assert_eq!(arguments.original_count, 0);
                let admission = engine.begin_original_allocator(owner, arguments).unwrap();
                assert!(
                    engine
                        .original_allocator_publication_admission(owner, &admission)
                        .is_err()
                );
                self.capture_pin = Some(clone_pin(event.pin));
                engine
                    .original_connect_provider_submitted(owner, &admission)
                    .unwrap();
                event.call = admission.call.native_command_call();
                self.metadata = Some(actual);
                self.engine = Some(engine);
                self.admission = Some(admission);
            }
            3 => {
                let owner = self.owner.unwrap();
                let admission = self.admission.as_ref().unwrap();
                let engine = self.engine.as_mut().unwrap();
                engine
                    .original_call_prepared(owner, admission, None, event.command)
                    .unwrap();
                engine.original_connect_invoked(owner, admission).unwrap();
                assert!(
                    engine
                        .original_allocator_publication_admission(owner, admission)
                        .is_err()
                );
            }
            4 => {
                self.retain(event);
                assert!(!event.original.is_null() && !event.result.is_null());
                let original: wire::OriginalResult = unsafe { *event.original }.into();
                let result: wire::CommandResult = unsafe { *event.result }.into();
                println!(
                    "OPENAT_PUBLICATION_RECEIPT {}",
                    serde_json::to_string(&(result.clone(), original.clone())).unwrap()
                );
                let selected = &original.selection;
                let owner = self.owner.unwrap();
                let admission = self.admission.as_ref().unwrap();
                assert_eq!(selected.call, admission.call.native_command_call());
                assert_eq!(selected.owner_mm, owner.mm.generation());
                assert_eq!(selected.command, event.command);
                assert_eq!(result.operation, 18);
                assert_eq!(result.phase, 1);
                assert_eq!(i64::from(result.returned), i64::from(original.returned));
                allocator_result(
                    &original,
                    i64::from(original.returned),
                    Kind::Openat,
                    event.count,
                )
                .unwrap()
                .unwrap();
                let engine = self.engine.as_mut().unwrap();
                engine
                    .original_connect_selected(
                        owner,
                        admission,
                        selected.command,
                        (
                            selected.provider,
                            selected.task,
                            selected.task_start,
                            selected.table,
                            selected.file,
                        ),
                    )
                    .unwrap();
                engine
                    .original_connect_returned(owner, admission, i64::from(original.returned))
                    .unwrap();
                assert!(
                    engine
                        .original_allocator_publication_admission(owner, admission)
                        .is_err()
                );
                assert!(engine.finish_original_connect(owner, admission).is_err());
                self.original = Some(original);
            }
            5 => {
                // C has collected and ACKed this exact original command. Close
                // the actual duplicated PIDFD before admitting the table permit.
                let owner = self.owner.unwrap();
                let admission = self.admission.as_ref().unwrap();
                let engine = self.engine.as_mut().unwrap();
                let raw = i64::from(self.original.as_ref().unwrap().returned);
                engine
                    .original_connect_provider_retired(owner, admission, raw)
                    .unwrap();
                assert!(
                    engine
                        .original_allocator_publication_admission(owner, admission)
                        .is_err()
                );
                let pin = self.capture_pin.take().unwrap().into_raw_fd();
                assert_eq!(unsafe { libc::close(pin) }, 0);
                assert_eq!(unsafe { libc::fcntl(pin, libc::F_GETFD) }, -1);
                assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
                engine
                    .original_connect_pin_released(owner, admission)
                    .unwrap();
                let mutation = engine
                    .original_allocator_publication_admission(owner, admission)
                    .unwrap();
                assert_eq!(
                    engine
                        .original_allocator_publication_admission(owner, admission)
                        .unwrap(),
                    mutation
                );
                assert!(engine.finish_original_connect(owner, admission).is_err());
                self.mutation = Some(mutation);
                println!(
                    "OPENAT_PUBLICATION_ORDER native_ack_before_pin_close=1 pin_closed_before_permit=1 unresolved_before_publication=1"
                );
            }
            6 => {
                self.retain(event);
                let owner = self.owner.unwrap();
                let admission = self.admission.as_ref().unwrap();
                let mutation = self.mutation.as_ref().unwrap();
                let original = self.original.as_ref().unwrap();
                let selected = &original.selection;
                let (begin, end, fd) = allocator_result(
                    original,
                    i64::from(original.returned),
                    Kind::Openat,
                    event.count,
                )
                .unwrap()
                .unwrap();
                let actual = self.metadata.as_ref().unwrap();
                let mut receipt = Installation::checked(
                    Owner {
                        owner,
                        metadata: actual.clone(),
                        files: admission.arguments.files,
                        provider: selected.provider,
                        task: selected.task,
                        start: selected.task_start,
                        table: selected.table,
                    },
                    mutation.publication.permit,
                    Source::Openat(admission.call),
                    (event.command, fd, selected.file),
                    (begin, end, event.through),
                    &self.history,
                )
                .unwrap();
                receipt.allocated = Some(allocated_profile(original).unwrap());
                let receipt = receipt.reconcile(&self.history).unwrap();
                assert!(!receipt.removed_before_publication());
                let profile = allocated_profile(original).unwrap();
                assert_eq!(profile.kind().unwrap(), crate::fd::FdType::Regular);
                assert!(!event.opened_stat.is_null());
                let stat = unsafe { *event.opened_stat };
                assert_eq!(stat.st_mode, profile.mode);
                let engine = self.engine.as_mut().unwrap();
                engine
                    .confirm_original_allocator_publication_result(owner, admission)
                    .unwrap();
                let binding = engine
                    .publish_original_openat_installation(
                        owner,
                        &mutation.publication,
                        &receipt,
                        (actual, &mut actual.lock().unwrap()),
                        (
                            OpenatEnrollment {
                                kind: profile.kind().unwrap(),
                                status_flags: profile.status_flags,
                            },
                            Some(stat.into()),
                        ),
                        LogicalTime::ZERO,
                    )
                    .unwrap();
                {
                    let metadata = actual.lock().unwrap();
                    assert_eq!(metadata.descriptor_binding(fd).unwrap(), binding);
                    let entry = metadata.file_handles.get(&fd).unwrap();
                    assert_eq!(entry.ty(), crate::fd::FdType::Regular);
                    assert_eq!(entry.status_flags(), profile.status_flags);
                    assert!(entry.is_cloexec());
                }
                engine
                    .original_allocator_publication_finished(
                        owner,
                        admission,
                        mutation.publication.permit,
                    )
                    .unwrap();
                engine.finish_original_connect(owner, admission).unwrap();
                assert!(engine.finish_original_connect(owner, admission).is_err());
                println!(
                    "OPENAT_PUBLICATION_RUST binding={} native_call={} metadata_ack=1 server_ack=1 original_finished=1",
                    serde_json::to_string(&binding).unwrap(),
                    admission.call.native_command_call()
                );
                self.receipt = Some(receipt);
            }
            7 => {
                let fd = self.original.as_ref().unwrap().returned;
                let metadata = self.metadata.as_ref().unwrap().lock().unwrap();
                assert_eq!(metadata.descriptor_binding(fd).unwrap().slot.fd, fd);
                assert!(metadata.file_handles.get(&fd).unwrap().is_cloexec());
                let mut pin = libc::pollfd {
                    fd: event.pin,
                    events: libc::POLLIN,
                    revents: 0,
                };
                assert_eq!(unsafe { libc::poll(&mut pin, 1, 0) }, 0);
                println!(
                    "OPENAT_PUBLICATION_LIVE actual_getfd_after_server_ack=1 same_inode=1 owner_pidfd_live=1"
                );
            }
            8 => {
                self.retain(event);
                let original = self.original.as_ref().unwrap();
                let (_, end, fd) = allocator_result(
                    original,
                    i64::from(original.returned),
                    Kind::Openat,
                    event.count,
                )
                .unwrap()
                .unwrap();
                let mut removed = false;
                let mut retired = false;
                for ordinal in end + 1..=event.through {
                    match self.history.transition(ordinal).unwrap() {
                        Some(Transition::Remove { end, .. })
                            if end.table == original.selection.table
                                && end.fd == fd
                                && end.file == original.selection.file =>
                        {
                            removed = true
                        }
                        Some(Transition::FileRetired(row))
                            if row.file == original.selection.file =>
                        {
                            retired = true
                        }
                        _ => {}
                    }
                }
                assert!(removed && retired);
                let owner = self.owner.unwrap();
                let engine = self.engine.as_mut().unwrap();
                engine.retire_fd_table_owner(owner);
                engine.stream_owner_gone(owner);
                engine.finish_fd_mutations().unwrap();
                println!(
                    "OPENAT_PUBLICATION_TERMINAL exact_remove=1 final_file_release=1 no_unresolved_call_or_permit=1"
                );
            }
            _ => panic!("unexpected native stage"),
        }
        self.stages.push(event.stage);
    }
}
unsafe extern "C" fn callback(context: *mut c_void, event: *mut Event) -> i32 {
    let context = unsafe { &mut *context.cast::<Context>() };
    let event = unsafe { &mut *event };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| context.step(event))) {
        Ok(()) => 0,
        Err(_) => {
            context.failed = true;
            eprintln!("OPENAT_PUBLICATION_RUST_FAILURE stage={}", event.stage);
            -1
        }
    }
}

#[test]
#[ignore = "requires the sealed bounded native Openat publication runner and real BPF owner"]
fn native_openat_receipt_publishes_same_call_before_guest_close() {
    println!(); // Keep exact native receipts on their own bounded log lines.
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let value = |name| {
        CString::new(std::env::var(name).expect("sealed native runner environment")).unwrap()
    };
    let shim = value("HERMIT_NATIVE_OPENAT_PUBLICATION_SHIM");
    let object = value("HERMIT_NATIVE_OPENAT_PUBLICATION_OBJECT");
    let path = value("HERMIT_NATIVE_OPENAT_PUBLICATION_FILE");
    let uid: u32 = std::env::var("HERMIT_NATIVE_OPENAT_PUBLICATION_UID")
        .unwrap()
        .parse()
        .unwrap();
    let gid: u32 = std::env::var("HERMIT_NATIVE_OPENAT_PUBLICATION_GID")
        .unwrap()
        .parse()
        .unwrap();
    type Bridge = unsafe extern "C" fn(
        *const libc::c_char,
        *const libc::c_char,
        u32,
        u32,
        unsafe extern "C" fn(*mut c_void, *mut Event) -> i32,
        *mut c_void,
    ) -> i32;
    let library = unsafe { libc::dlopen(shim.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    assert!(!library.is_null(), "sealed native fixture dlopen failed");
    let name = CString::new("hermit_openat_publication_bridge").unwrap();
    let address = unsafe { libc::dlsym(library, name.as_ptr()) };
    assert!(!address.is_null());
    let bridge: Bridge = unsafe { std::mem::transmute(address) };
    let mut context = Context::default();
    let result = unsafe {
        bridge(
            object.as_ptr(),
            path.as_ptr(),
            uid,
            gid,
            callback,
            (&mut context as *mut Context).cast(),
        )
    };
    assert_eq!(unsafe { libc::dlclose(library) }, 0);
    assert_eq!(result, 0);
    assert!(!context.failed);
    assert_eq!(context.stages, (0..=9).collect::<Vec<_>>());
}
