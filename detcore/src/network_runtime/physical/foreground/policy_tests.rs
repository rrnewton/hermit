//! Component premises: controlled initial census and provider wire responses,
//! with /dev/null descriptor stand-ins. No kernel/BPF/DSR qualification. Child
//! authority is nevertheless issued by the real retained prepare/observe and
//! bind consumers, never by constructing NativeBirthAdmission or a child root.
use std::os::fd::AsFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::time::Duration;
use std::time::Instant;

use reverie::syscalls::CloneFlags;

use super::*;
use crate::network_replay::NetworkFdPublicationPermit;
use crate::network_replay::NetworkStreamLeaseId;
use crate::network_runtime::NetworkRuntimeOwner;
use crate::network_runtime::NetworkRuntimeResources;
use crate::network_runtime::ProviderWireFormat;
use crate::network_runtime::accepted_provider::CallStatus;
use crate::network_runtime::accepted_provider::Observation;
use crate::network_runtime::accepted_provider::Reply;
use crate::network_runtime::accepted_provider::Request;
use crate::network_runtime::accepted_transport::AcceptedSession;
use crate::network_runtime::accepted_transport::Received;
use crate::network_runtime::native_birth::NativeBirthAdmission;

pub(crate) struct SharedBirthFixture {
    pub(crate) runtime: NetworkRuntimeResources,
    pub(crate) parent: Arc<ForegroundRoot>,
    pub(crate) child: Arc<ForegroundRoot>,
    // Keep the actual issued birth admission alive in the same field/drop order.
    pub(crate) _birth: NativeBirthAdmission,
    pub(crate) metadata: Arc<Mutex<FileMetadata>>,
    pub(crate) memory: Arc<Mutex<MemoryMetadata>>,
    _peer: BirthPeer,
}

#[derive(Clone, Copy)]
enum BirthMutation {
    None,
    Provider,
    CreatorTask,
    CreatorStart,
    NonThread,
    CopiedMm,
    Vfork,
    Terminal,
}
struct BirthOptions {
    wire: ProviderWireFormat,
    mutation: BirthMutation,
}
impl From<ProviderWireFormat> for BirthOptions {
    fn from(wire: ProviderWireFormat) -> Self {
        Self {
            wire,
            mutation: BirthMutation::None,
        }
    }
}

struct BirthPeer {
    owner: NetworkRuntimeOwner,
    peer: AcceptedSession,
}
impl Drop for BirthPeer {
    fn drop(&mut self) {
        if let Some(Ok(driver)) = self.owner.shared.driver.lock().unwrap().as_mut() {
            let _ = driver.stop_and_join(Instant::now() + Duration::from_secs(2));
        }
    }
}
impl BirthPeer {
    async fn receive(&mut self) -> (u64, Request) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(Received::Request(sequence)) = self.peer.try_receive().unwrap() {
                    let (envelope, _, _) = self.peer.retained_request(sequence).unwrap();
                    return (sequence, serde_json::from_slice(&envelope.body).unwrap());
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("bounded retained birth request")
    }
    fn reply(&mut self, sequence: u64, reply: Reply) {
        self.peer
            .dispatch(sequence, |_, _| {
                serde_json::to_vec(&reply).map_err(std::io::Error::other)
            })
            .unwrap();
        assert!(self.peer.try_reply(sequence).unwrap());
    }
}
fn status(operation: &str) -> CallStatus {
    CallStatus {
        operation: operation.into(),
        returned: 0,
        errno: None,
    }
}
impl SharedBirthFixture {
    /// Move the unique runtime into GlobalState while keeping the same issued
    /// birth and responder alive. No runtime or semantic authority is cloned.
    pub(crate) fn into_runtime_and_retention(self) -> (NetworkRuntimeResources, Box<dyn std::any::Any>) {
        (self.runtime, Box::new((self._birth, self._peer)))
    }
    pub(crate) fn into_runtime_and_profile_service(
        self,
    ) -> (
        NetworkRuntimeResources,
        Box<dyn std::any::Any>,
        crate::network_runtime::current_close_profile::tests::Service,
    ) {
        let service = crate::network_runtime::current_close_profile::tests::serve_existing(
            &self.runtime,
            self.parent,
            self._peer,
            |peer| &mut peer.peer,
        );
        (self.runtime, Box::new(self._birth), service)
    }
    pub(crate) async fn new_after_setup_with_profile(
        thread: i32,
        setup: impl FnOnce(&Arc<ForegroundRoot>, &InitialTableClaim),
    ) -> Self {
        Self::new_with_observer_and_wire(
            thread,
            Some(libc::SYS_clone as i32),
            |root, claim| {
                setup(root, claim);
                None
            },
            |_, _, _| {},
            |runtime, _, _| Some(runtime),
            ProviderWireFormat::Abi12Copy5,
        )
        .await
        .expect("profile fixture retains the original birth path")
    }
    pub(crate) async fn new(thread: i32) -> Self {
        Self::new_after_entry(thread, |_, _, _| {}).await
    }
    pub(crate) async fn new_after_entry(
        thread: i32,
        before: impl FnOnce(
            &NetworkRuntimeResources,
            &Arc<ForegroundRoot>,
            &crate::network_runtime::JoinedNativePrefix,
        ),
    ) -> Self {
        Self::new_with_setup(thread, |_, _| {}, before).await
    }
    pub(crate) async fn new_after_setup(
        thread: i32,
        setup: impl FnOnce(&Arc<ForegroundRoot>, &InitialTableClaim),
    ) -> Self {
        Self::new_with_setup(thread, setup, |_, _, _| {}).await
    }
    async fn new_with_setup(
        thread: i32,
        setup: impl FnOnce(&Arc<ForegroundRoot>, &InitialTableClaim),
        before: impl FnOnce(
            &NetworkRuntimeResources,
            &Arc<ForegroundRoot>,
            &crate::network_runtime::JoinedNativePrefix,
        ),
    ) -> Self {
        Self::new_with_observer(
            thread,
            Some(libc::SYS_clone as i32),
            |root, claim| {
                setup(root, claim);
                None
            },
            before,
            |runtime, _, _| Some(runtime),
        )
        .await
        .expect("default birth fixture continues after preparation")
    }
    pub(crate) async fn new_with_observer(
        thread: i32,
        syscall: Option<i32>,
        setup: impl FnOnce(
            &Arc<ForegroundRoot>,
            &InitialTableClaim,
        ) -> Option<NetworkFdPublicationPermit>,
        before: impl FnOnce(
            &NetworkRuntimeResources,
            &Arc<ForegroundRoot>,
            &crate::network_runtime::JoinedNativePrefix,
        ),
        observe: impl FnOnce(
            NetworkRuntimeResources,
            &Arc<ForegroundRoot>,
            NetworkFdPublicationPermit,
        ) -> Option<NetworkRuntimeResources>,
    ) -> Option<Self> {
        Self::new_with_observer_and_wire(
            thread,
            syscall,
            setup,
            before,
            observe,
            ProviderWireFormat::Abi7Copy4,
        )
        .await
    }
    async fn new_with_observer_and_wire(
        thread: i32,
        syscall: Option<i32>,
        setup: impl FnOnce(
            &Arc<ForegroundRoot>,
            &InitialTableClaim,
        ) -> Option<NetworkFdPublicationPermit>,
        before: impl FnOnce(
            &NetworkRuntimeResources,
            &Arc<ForegroundRoot>,
            &crate::network_runtime::JoinedNativePrefix,
        ),
        observe: impl FnOnce(
            NetworkRuntimeResources,
            &Arc<ForegroundRoot>,
            NetworkFdPublicationPermit,
        ) -> Option<NetworkRuntimeResources>,
        wire: ProviderWireFormat,
    ) -> Option<Self> {
        Self::new_with_retention_observer(
            thread,
            syscall,
            setup,
            before,
            observe,
            wire,
            |runtime, _, birth, pin| {
                runtime.retain_native_child(birth, pin).unwrap();
                true
            },
        )
        .await
    }
    async fn new_with_retention_observer(
        thread: i32,
        syscall: Option<i32>,
        setup: impl FnOnce(
            &Arc<ForegroundRoot>,
            &InitialTableClaim,
        ) -> Option<NetworkFdPublicationPermit>,
        before: impl FnOnce(
            &NetworkRuntimeResources,
            &Arc<ForegroundRoot>,
            &crate::network_runtime::JoinedNativePrefix,
        ),
        observe: impl FnOnce(
            NetworkRuntimeResources,
            &Arc<ForegroundRoot>,
            NetworkFdPublicationPermit,
        ) -> Option<NetworkRuntimeResources>,
        options: impl Into<BirthOptions>,
        retain: impl FnOnce(
            &NetworkRuntimeResources,
            &Arc<ForegroundRoot>,
            &NativeBirthAdmission,
            std::os::fd::BorrowedFd<'_>,
        ) -> bool,
    ) -> Option<Self> {
        let options = options.into();
        let wire = options.wire;
        let terminal = matches!(options.mutation, BirthMutation::Terminal);
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        // This local exclusive pair is a controlled startup premise.
        let (owner, runtime) = unsafe {
            NetworkRuntimeResources::from_authenticated_startup(
                OwnedFd::from_raw_fd(pair[0]),
                [184; 16],
                wire,
            )
        };
        let mut peer = BirthPeer {
            owner,
            peer: AcceptedSession::from_wire(
                unsafe { OwnedFd::from_raw_fd(pair[1]) },
                [184; 16],
                wire,
            )
            .unwrap(),
        };
        let (mut tasks, parent_owner, metadata, memory, claim) = controlled_tasks(thread);
        tasks
            .bind_foreground_metadata(parent_owner, &metadata, &memory)
            .unwrap();
        let parent = tasks.foreground_root(parent_owner).unwrap();
        assert!(parent.is_sole_initial_root(parent_owner));
        let selected_permit = setup(&parent, &claim);
        let task = tasks.tasks.remove(&parent_owner.thread).unwrap();
        let pin: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        {
            let mut actual = runtime.shared.physical.lock().unwrap();
            actual.first_task = tasks.first_task;
            actual.next_registration = tasks.next_registration;
            actual.sole_initial_root_lost = tasks.sole_initial_root_lost.clone();
            actual.shared_mm_lineage_lost = tasks.shared_mm_lineage_lost.clone();
            actual.tasks.insert(
                parent_owner.thread,
                Task {
                    mm: task.mm,
                    process: task.process,
                    thread: task.thread,
                    handle: pin.try_clone().unwrap(),
                    initial_exec: task.initial_exec,
                    retired: task.retired,
                    enrollment: task.enrollment,
                    native_birth: task.native_birth,
                    foreground_metadata: task.foreground_metadata,
                    foreground_root: task.foreground_root,
                    shared_cleanup_requested: task.shared_cleanup_requested,
                    shared_terminal: task.shared_terminal,
                },
            );
        }
        let prefix = runtime
            .join_foreground_prefix(parent.clone())
            .await
            .unwrap();
        before(&runtime, &parent, &prefix);
        let permit = selected_permit.unwrap_or(NetworkFdPublicationPermit {
            owner: parent_owner,
            files: parent.files(),
            lease: NetworkStreamLeaseId::controlled_fixture(184),
        });
        let child_thread = DetTid::from_raw(thread.checked_add(1).unwrap());
        let mut flags = CloneFlags::CLONE_VM
            | CloneFlags::CLONE_FILES
            | CloneFlags::CLONE_SIGHAND
            | CloneFlags::CLONE_THREAD;
        match options.mutation {
            BirthMutation::NonThread => flags.remove(CloneFlags::CLONE_THREAD),
            BirthMutation::CopiedMm => {
                flags.remove(
                    CloneFlags::CLONE_THREAD | CloneFlags::CLONE_SIGHAND | CloneFlags::CLONE_VM,
                );
            }
            BirthMutation::Vfork => flags.insert(CloneFlags::CLONE_VFORK),
            _ => {}
        }
        let command = permit.native_command_call() + 17;
        let prepared_request = if let Some(syscall) = syscall {
            let prepare = runtime.prepare_native_birth(permit, syscall);
            let self_syscall = syscall;
            let respond = async {
                let (sequence, request) = peer.receive().await;
                assert!(
                    matches!(request, Request::PrepareNativeBirth { call, mm, table, syscall }
                if call == permit.native_command_call() && mm == parent_owner.mm.generation()
                && table == parent.association().table() && syscall == self_syscall)
                );
                peer.reply(
                    sequence,
                    Reply::Prepared(Observation {
                        status: status("ap_prepare_native_birth"),
                        raw: command,
                    }),
                );
                sequence
            };
            let (prepared, prepared_request) =
                tokio::time::timeout(Duration::from_secs(2), async {
                    tokio::join!(prepare, respond)
                })
                .await
                .expect("bounded birth preparation");
            prepared.unwrap();
            prepared_request
        } else {
            0
        };
        // Explicit negative-fixture stop: no child observation or ready claim.
        let runtime = observe(runtime, &parent, permit)?;
        assert!(
            syscall.is_some(),
            "an unprepared negative fixture cannot continue to child observation"
        );
        let observe = runtime.observe_native_birth(
            permit,
            child_thread,
            parent_owner.thread,
            pin.as_fd(),
            terminal,
            flags,
        );
        let respond = async {
            let (sequence, request) = peer.receive().await;
            assert!(matches!(request, Request::ObserveNativeBirth {
                call, command: actual, prepared_request: original, child, terminal: observed_terminal }
                if observed_terminal == terminal && call == permit.native_command_call() && actual == command
                && original == prepared_request && child == child_thread.as_raw()));
            let (provider, creator_task, creator_start) =
                parent.association().native_root().unwrap();
            let same_group = flags.contains(CloneFlags::CLONE_THREAD);
            let raw_child = (creator_task as u32 + 1) as u64;
            let mut raw = crate::network_runtime::accepted_provider_ffi::NativeBirth {
                command,
                call: permit.native_command_call(),
                owner_mm: parent_owner.mm.generation(),
                provider,
                creator_task,
                creator_start,
                creator_table: parent.association().table(),
                child_task: if same_group {
                    (creator_task & !0xffff_ffff) | raw_child
                } else {
                    (raw_child << 32) | raw_child
                },
                child_start: creator_start + 1,
                child_table: parent.association().table(),
                parent_task: creator_task,
                parent_start: creator_start,
                kernel_flags: flags.bits(),
                shared_mm: u32::from(flags.contains(CloneFlags::CLONE_VM)),
                shared_files: 1,
                same_thread_group: u32::from(same_group),
                exit_signal: -1,
                requested_exit_signal: 0,
                ready: 1,
                pidfd_fd: -1,
                ..Default::default()
            };
            match options.mutation {
                BirthMutation::Provider => raw.provider += 1,
                BirthMutation::CreatorTask => raw.creator_task ^= 2,
                BirthMutation::CreatorStart => raw.creator_start += 1,
                _ => {}
            }
            peer.reply(
                sequence,
                Reply::NativeBirth(Observation {
                    status: status("ap_admit_native_birth_child"),
                    raw: raw.into(),
                }),
            );
        };
        let (birth, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(observe, respond)
        })
        .await
        .expect("bounded child observation");
        let birth = birth.unwrap();
        if !retain(&runtime, &parent, &birth, pin.as_fd()) {
            return None;
        }
        runtime
            .bind_foreground_metadata(birth.child_owner(), &metadata, &memory)
            .unwrap();
        let child = runtime.foreground_root(birth.child_owner()).unwrap();
        assert_eq!(
            runtime
                .shared
                .physical
                .lock()
                .unwrap()
                .native_birth(child.owner())
                .unwrap(),
            birth
        );
        assert!(parent.is_current(parent_owner));
        assert!(child.is_current(child.owner()));
        assert!(parent.same_memory_authority(&child));
        assert!(!parent.is_sole_initial_root(parent_owner));
        assert!(!child.is_sole_initial_root(child.owner()));
        Some(Self {
            runtime,
            parent,
            child,
            _birth: birth,
            metadata,
            memory,
            _peer: peer,
        })
    }
}

async fn invalidate_child(transition: u8) {
    let f = SharedBirthFixture::new(61).await;
    let owner = f.child.owner();
    let prefix = f
        .runtime
        .join_foreground_prefix(f.child.clone())
        .await
        .unwrap();
    let parent_prefix = f
        .runtime
        .join_foreground_prefix(f.parent.clone())
        .await
        .unwrap();
    let next = NetworkStreamOwner {
        mm: owner.mm.for_exec(owner.thread),
        ..owner
    };
    {
        let mut tasks = f.runtime.shared.physical.lock().unwrap();
        match transition {
            0 => tasks
                .register(
                    next,
                    61,
                    62,
                    || Ok(std::fs::File::open("/dev/null")?.into()),
                )
                .unwrap(),
            1 => tasks.close_native_preparations(owner).unwrap(),
            2 => tasks.forget(owner),
            3 => {
                tasks.forget(owner);
                tasks
                    .register(
                        next,
                        61,
                        62,
                        || Ok(std::fs::File::open("/dev/null")?.into()),
                    )
                    .unwrap();
                assert!(
                    tasks.foreground_root(next).is_err(),
                    "numeric reuse inherited an old root"
                );
            }
            _ => unreachable!(),
        }
        assert!(tasks.foreground_root(owner).is_err());
    }
    assert!(!f.child.is_current(owner));
    assert!(f.child.metadata().is_err());
    assert!(f.child.memory().is_err());
    assert!(f.runtime.validate_foreground_prefix(&prefix).is_err());
    assert!(
        f.runtime
            .with_foreground_prefix(&prefix, |_| Ok(()))
            .is_err()
    );
    assert!(f.parent.is_current(f.parent.owner()));
    f.runtime
        .validate_foreground_prefix(&parent_prefix)
        .unwrap();
}
#[tokio::test]
async fn child_root_replacement_revokes_exact_root_and_prefix() {
    invalidate_child(0).await;
}
#[tokio::test]
async fn child_root_final_wait_revokes_exact_root_and_prefix() {
    invalidate_child(1).await;
}
#[tokio::test]
async fn child_root_forget_revokes_exact_root_and_prefix() {
    invalidate_child(2).await;
}
#[tokio::test]
async fn child_root_numeric_reuse_cannot_restore_old_authority() {
    invalidate_child(3).await;
}

#[tokio::test]
async fn child_root_failed_opener_preserves_exact_root_and_prefix() {
    let f = SharedBirthFixture::new(61).await;
    let owner = f.child.owner();
    let prefix = f
        .runtime
        .join_foreground_prefix(f.child.clone())
        .await
        .unwrap();
    let next = NetworkStreamOwner {
        mm: owner.mm.for_exec(owner.thread),
        ..owner
    };
    let error = f
        .runtime
        .shared
        .physical
        .lock()
        .unwrap()
        .register(next, 61, 62, || {
            Err(std::io::Error::other("controlled opener failure"))
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "controlled opener failure");
    assert!(f.child.is_current(owner));
    assert!(Arc::ptr_eq(
        &f.child,
        &f.runtime.foreground_root(owner).unwrap()
    ));
    f.runtime.validate_foreground_prefix(&prefix).unwrap();
    f.runtime
        .with_foreground_prefix(&prefix, |_| Ok(()))
        .unwrap();
}

#[tokio::test]
async fn child_root_survives_creator_final_wait_and_forget() {
    let f = SharedBirthFixture::new(61).await;
    let prefix = f
        .runtime
        .join_foreground_prefix(f.child.clone())
        .await
        .unwrap();
    {
        let mut tasks = f.runtime.shared.physical.lock().unwrap();
        tasks.close_native_preparations(f.parent.owner()).unwrap();
        tasks.forget(f.parent.owner());
    }
    assert!(!f.parent.is_current(f.parent.owner()));
    assert!(f.child.is_current(f.child.owner()));
    assert!(Arc::ptr_eq(
        &f.child,
        &f.runtime.foreground_root(f.child.owner()).unwrap()
    ));
    assert!(Arc::ptr_eq(&f.metadata, &f.child.metadata().unwrap()));
    assert!(Arc::ptr_eq(&f.memory, &f.child.memory().unwrap()));
    f.runtime.validate_foreground_prefix(&prefix).unwrap();
    f.runtime
        .with_foreground_prefix(&prefix, |_| Ok(()))
        .unwrap();
}

fn invalidate_initial(transition: u8) {
    let (mut tasks, owner, metadata, memory, _) = controlled_tasks(61);
    tasks
        .bind_foreground_metadata(owner, &metadata, &memory)
        .unwrap();
    let root = tasks.foreground_root(owner).unwrap();
    let next = NetworkStreamOwner {
        mm: owner.mm.for_exec(owner.thread),
        ..owner
    };
    match transition {
        0 => tasks.register(next, 61, 61, || Ok(102)).unwrap(),
        1 => tasks.close_native_preparations(owner).unwrap(),
        2 => tasks.forget(owner),
        3 => {
            tasks.forget(owner);
            tasks.register(next, 61, 61, || Ok(102)).unwrap();
            assert!(tasks.foreground_root(next).is_err());
        }
        _ => unreachable!(),
    }
    assert!(!root.is_current(owner));
    assert!(tasks.foreground_root(owner).is_err());
    assert_eq!(
        root.has_sole_initial_root_history(),
        matches!(transition, 1 | 2)
    );
}
#[test]
fn initial_root_replacement_revokes_authority() {
    invalidate_initial(0);
}
#[test]
fn initial_root_final_wait_revokes_authority() {
    invalidate_initial(1);
}
#[test]
fn initial_root_forget_revokes_authority() {
    invalidate_initial(2);
}
#[test]
fn initial_root_numeric_reuse_cannot_restore_authority() {
    invalidate_initial(3);
}

#[test]
fn initial_root_failed_opener_preserves_sole_policy() {
    let (mut tasks, owner, metadata, memory, _) = controlled_tasks(61);
    tasks
        .bind_foreground_metadata(owner, &metadata, &memory)
        .unwrap();
    let root = tasks.foreground_root(owner).unwrap();
    let next = NetworkStreamOwner {
        mm: owner.mm.for_exec(owner.thread),
        ..owner
    };
    assert_eq!(
        tasks
            .register(next, 61, 61, || Err(std::io::Error::other(
                "controlled opener failure"
            )))
            .unwrap_err()
            .to_string(),
        "controlled opener failure"
    );
    assert!(root.is_sole_initial_root(owner));
    assert!(Arc::ptr_eq(&root, &tasks.foreground_root(owner).unwrap()));
}

#[tokio::test]
async fn shared_attempt_census_accepts_real_retained_birth_without_restoring_sole_history() {
    let f = SharedBirthFixture::new(61).await;
    for root in [&f.parent, &f.child] {
        assert!(root.has_shared_mm_history());
        assert!(!root.has_sole_initial_root_history());
        f.runtime.with_shared_foreground_lineage(root.owner(), |lineage| {
            assert!(Arc::ptr_eq(lineage.root(), root));
            assert_eq!(lineage.members().count(), 2);
            Ok(())
        }).unwrap();
    }
    {
        let mut tasks = f.runtime.shared.physical.lock().unwrap();
        tasks.close_native_preparations(f.parent.owner()).unwrap();
        tasks.forget(f.parent.owner());
    }
    assert!(!f.parent.is_current(f.parent.owner()));
    assert!(f.child.has_shared_mm_history());
    f.runtime.with_shared_foreground_lineage(f.child.owner(), |lineage| {
        assert_eq!(lineage.members().count(), 1);
        Ok(())
    }).unwrap();
    assert!(!f.child.has_sole_initial_root_history());
}

#[tokio::test]
async fn shared_attempt_census_unknown_registration_cannot_be_erased() {
    let f = SharedBirthFixture::new(61).await;
    let unknown = NetworkStreamOwner { thread: DetTid::from_raw(63), mm: f.parent.owner().mm };
    {
        let mut tasks = f.runtime.shared.physical.lock().unwrap();
        tasks.register(unknown, 61, 63, || Ok(std::fs::File::open("/dev/null")?.into())).unwrap();
    }
    assert!(f.runtime.with_shared_foreground_lineage(f.parent.owner(), |_| Ok(())).is_err());
    f.runtime.shared.physical.lock().unwrap().forget(unknown);
    assert!(!f.parent.has_shared_mm_history());
    assert!(!f.child.has_shared_mm_history());
    assert!(f.runtime.with_shared_foreground_lineage(f.parent.owner(), |_| Ok(())).is_err());
}

#[tokio::test]
async fn shared_attempt_history_replacement_and_generic_revoke_are_sticky() {
    for replacement in [false, true] {
        let f = SharedBirthFixture::new(61).await;
        let mut tasks = f.runtime.shared.physical.lock().unwrap();
        if replacement {
            let owner = f.child.owner();
            tasks.register(NetworkStreamOwner { mm: owner.mm.for_exec(owner.thread), ..owner }, 61, 62,
                || Ok(std::fs::File::open("/dev/null")?.into())).unwrap();
        } else {
            tasks.revoke_foreground_lineage();
        }
        assert!(!f.parent.has_shared_mm_history());
        assert!(!f.child.has_shared_mm_history());
        assert!(tasks.shared_foreground_lineage(f.parent.owner()).is_err());
    }
}

// The existing fixture used to bind child metadata before exposing its census.
// This boundary preserves actual retained birth but deliberately withholds the
// state-ready observation, as when ParentContinue chooses the parent first.
#[tokio::test]
async fn shared_birth_retention_completes_parent_census_before_child_ready() {
    let f = SharedBirthFixture::new_with_retention_observer(
        61,
        Some(libc::SYS_clone3 as i32),
        |_, _| None,
        |_, _, _| {},
        |runtime, _, _| Some(runtime),
        ProviderWireFormat::Abi7Copy4,
        |runtime, parent, birth, pin| {
            runtime.retain_native_child(birth, pin).unwrap();
            let observed = runtime.with_shared_foreground_lineage(parent.owner(), |lineage| {
                assert_eq!(lineage.members().count(), 2);
                let child = lineage
                    .members()
                    .find(|root| root.owner() == birth.child_owner())
                    .expect("actual retained child belongs to the complete census");
                assert!(child.same_memory_authority(parent));
                assert!(!child.has_sole_initial_root_history());
                assert!(!parent.has_sole_initial_root_history());
                Ok(())
            });
            assert!(
                observed.is_ok(),
                "authenticated shared birth must complete parent census before child ready: {observed:?}"
            );
            true
        },
    )
    .await
    .unwrap();
    assert!(f.parent.same_memory_authority(&f.child));
}

#[tokio::test]
async fn inherited_birth_rejects_current_creator_identity_changes_without_losing_birth() {
    for mutation in [
        BirthMutation::Provider,
        BirthMutation::CreatorTask,
        BirthMutation::CreatorStart,
    ] {
        let result = SharedBirthFixture::new_with_retention_observer(
            61,
            Some(libc::SYS_clone3 as i32),
            |_, _| None,
            |_, _, _| {},
            |runtime, _, _| Some(runtime),
            BirthOptions {
                wire: ProviderWireFormat::Abi7Copy4,
                mutation,
            },
            |runtime, parent, birth, pin| {
                assert!(runtime.retain_native_child(birth, pin).is_err());
                let tasks = runtime.shared.physical.lock().unwrap();
                assert_eq!(tasks.native_birth(birth.child_owner()).unwrap(), *birth);
                assert!(tasks.foreground_root(birth.child_owner()).is_err());
                assert!(tasks.shared_foreground_lineage(parent.owner()).is_err());
                false
            },
        )
        .await;
        assert!(result.is_none());
    }
}

#[tokio::test]
async fn inherited_birth_keeps_unsupported_and_terminal_admission_without_issuing_shared_root() {
    for mutation in [
        BirthMutation::NonThread,
        BirthMutation::CopiedMm,
        BirthMutation::Vfork,
        BirthMutation::Terminal,
    ] {
        let result = SharedBirthFixture::new_with_retention_observer(
            61,
            Some(libc::SYS_clone3 as i32),
            |_, _| None,
            |_, _, _| {},
            |runtime, _, _| Some(runtime),
            BirthOptions {
                wire: ProviderWireFormat::Abi7Copy4,
                mutation,
            },
            |runtime, _, birth, pin| {
                runtime.retain_native_child(birth, pin).unwrap();
                let tasks = runtime.shared.physical.lock().unwrap();
                assert!(tasks.foreground_root(birth.child_owner()).is_err());
                if birth.terminal() {
                    assert!(tasks.get(birth.child_owner()).is_err());
                } else {
                    assert_eq!(tasks.native_birth(birth.child_owner()).unwrap(), *birth);
                }
                false
            },
        )
        .await;
        assert!(result.is_none());
    }
}

#[tokio::test]
async fn inherited_birth_does_not_overwrite_metadata_first_fd_or_mm_identity() {
    for wrong_memory in [false, true] {
        let result = SharedBirthFixture::new_with_retention_observer(
            61,
            Some(libc::SYS_clone3 as i32),
            |_, _| None,
            |_, _, _| {},
            |runtime, _, _| Some(runtime),
            ProviderWireFormat::Abi7Copy4,
            |runtime, parent, birth, pin| {
                let original_files = parent.metadata().unwrap();
                let original_mm = parent.memory().unwrap();
                let files = if wrong_memory {
                    original_files.clone()
                } else {
                    Arc::new(Mutex::new(
                        original_files
                            .lock()
                            .unwrap()
                            .fork_for(birth.child_owner().thread),
                    ))
                };
                let memory = if wrong_memory {
                    Arc::new(Mutex::new(original_mm.lock().unwrap().clone()))
                } else {
                    original_mm.clone()
                };
                {
                    let mut tasks = runtime.shared.physical.lock().unwrap();
                    tasks
                        .register(
                            birth.child_owner(),
                            birth.child_process().as_raw(),
                            birth.child_owner().thread.as_raw(),
                            || pin.try_clone_to_owned(),
                        )
                        .unwrap();
                }
                runtime
                    .bind_foreground_metadata(birth.child_owner(), &files, &memory)
                    .unwrap();
                assert!(runtime.retain_native_child(birth, pin).is_err());
                let tasks = runtime.shared.physical.lock().unwrap();
                assert_eq!(tasks.native_birth(birth.child_owner()).unwrap(), *birth);
                assert!(tasks.foreground_root(birth.child_owner()).is_err());
                let (kept_files, kept_mm) = tasks.tasks[&birth.child_owner().thread]
                    .foreground_metadata
                    .as_ref()
                    .unwrap();
                assert!(kept_files.ptr_eq(&Arc::downgrade(&files)));
                assert!(kept_mm.ptr_eq(&Arc::downgrade(&memory)));
                false
            },
        )
        .await;
        assert!(result.is_none());
    }
}

#[tokio::test]
async fn inherited_birth_declines_early_root_after_creator_terminal_revoke_or_replacement() {
    for transition in 0..3 {
        let result = SharedBirthFixture::new_with_retention_observer(
            61,
            Some(libc::SYS_clone3 as i32),
            |_, _| None,
            |_, _, _| {},
            |runtime, _, _| Some(runtime),
            ProviderWireFormat::Abi7Copy4,
            |runtime, parent, birth, pin| {
                {
                    let mut tasks = runtime.shared.physical.lock().unwrap();
                    match transition {
                        0 => tasks.close_native_preparations(parent.owner()).unwrap(),
                        1 => tasks.revoke_foreground_lineage(),
                        2 => tasks
                            .register(
                                NetworkStreamOwner {
                                    mm: parent.owner().mm.for_exec(parent.logical_process()),
                                    ..parent.owner()
                                },
                                parent.process(),
                                parent.thread(),
                                || pin.try_clone_to_owned(),
                            )
                            .unwrap(),
                        _ => unreachable!(),
                    }
                }
                runtime.retain_native_child(birth, pin).unwrap();
                let tasks = runtime.shared.physical.lock().unwrap();
                assert_eq!(tasks.native_birth(birth.child_owner()).unwrap(), *birth);
                assert!(tasks.foreground_root(birth.child_owner()).is_err());
                assert!(tasks.shared_foreground_lineage(parent.owner()).is_err());
                false
            },
        )
        .await;
        assert!(result.is_none());
    }
}

#[tokio::test]
async fn inherited_birth_ready_rechecks_arcs_and_identical_retention_survives_creator_exit() {
    let f = SharedBirthFixture::new(61).await;
    let wrong_files = Arc::new(Mutex::new(
        f.metadata.lock().unwrap().fork_for(f.child.owner().thread),
    ));
    let wrong_memory = Arc::new(Mutex::new(f.memory.lock().unwrap().clone()));
    assert!(
        f.runtime
            .bind_foreground_metadata(f.child.owner(), &wrong_files, &f.memory)
            .is_err()
    );
    assert!(
        f.runtime
            .bind_foreground_metadata(f.child.owner(), &f.metadata, &wrong_memory)
            .is_err()
    );
    f.runtime
        .bind_foreground_metadata(f.child.owner(), &f.metadata, &f.memory)
        .unwrap();
    let pin: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
    f.runtime
        .retain_native_child(&f._birth, pin.as_fd())
        .unwrap();
    assert!(Arc::ptr_eq(
        &f.child,
        &f.runtime.foreground_root(f.child.owner()).unwrap()
    ));
    f.runtime
        .shared
        .physical
        .lock()
        .unwrap()
        .close_native_preparations(f.parent.owner())
        .unwrap();
    f.runtime
        .retain_native_child(&f._birth, pin.as_fd())
        .unwrap();
    assert!(Arc::ptr_eq(
        &f.child,
        &f.runtime.foreground_root(f.child.owner()).unwrap()
    ));
}
