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
    #[expect(
        dead_code,
        reason = "retained pre-close shared-birth guard fixture; no active caller or qualification claim"
    )]
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
                ProviderWireFormat::Abi7Copy4,
            )
        };
        let mut peer = BirthPeer {
            owner,
            peer: AcceptedSession::new(unsafe { OwnedFd::from_raw_fd(pair[1]) }, [184; 16])
                .unwrap(),
        };
        let (mut tasks, parent_owner, metadata, memory, claim) = controlled_tasks(thread);
        tasks
            .bind_foreground_metadata(parent_owner, &metadata, &memory)
            .unwrap();
        let parent = tasks.foreground_root(parent_owner).unwrap();
        assert!(parent.is_sole_initial_root(parent_owner));
        setup(&parent, &claim);
        let task = tasks.tasks.remove(&parent_owner.thread).unwrap();
        let pin: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        {
            let mut actual = runtime.shared.physical.lock().unwrap();
            actual.first_task = tasks.first_task;
            actual.next_registration = tasks.next_registration;
            actual.sole_initial_root_lost = tasks.sole_initial_root_lost.clone();
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
                },
            );
        }
        let prefix = runtime
            .join_foreground_prefix(parent.clone())
            .await
            .unwrap();
        before(&runtime, &parent, &prefix);
        let permit = NetworkFdPublicationPermit {
            owner: parent_owner,
            files: parent.files(),
            lease: NetworkStreamLeaseId::controlled_fixture(184),
        };
        let child_thread = DetTid::from_raw(thread.checked_add(1).unwrap());
        let flags = CloneFlags::CLONE_VM
            | CloneFlags::CLONE_FILES
            | CloneFlags::CLONE_SIGHAND
            | CloneFlags::CLONE_THREAD;
        let command = permit.native_command_call() + 17;
        let prepare = runtime.prepare_native_birth(permit, libc::SYS_clone as i32);
        let respond = async {
            let (sequence, request) = peer.receive().await;
            assert!(
                matches!(request, Request::PrepareNativeBirth { call, mm, table, syscall }
                if call == permit.native_command_call() && mm == parent_owner.mm.generation()
                && table == parent.association().table() && syscall == libc::SYS_clone as i32)
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
        let (prepared, prepared_request) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(prepare, respond)
        })
        .await
        .expect("bounded birth preparation");
        prepared.unwrap();
        let observe = runtime.observe_native_birth(
            permit,
            child_thread,
            parent_owner.thread,
            pin.as_fd(),
            false,
            flags,
        );
        let respond = async {
            let (sequence, request) = peer.receive().await;
            assert!(matches!(request, Request::ObserveNativeBirth {
                call, command: actual, prepared_request: original, child, terminal: false }
                if call == permit.native_command_call() && actual == command
                && original == prepared_request && child == child_thread.as_raw()));
            let (provider, creator_task, creator_start) =
                parent.association().native_root().unwrap();
            let raw = crate::network_runtime::accepted_provider_ffi::NativeBirth {
                command,
                call: permit.native_command_call(),
                owner_mm: parent_owner.mm.generation(),
                provider,
                creator_task,
                creator_start,
                creator_table: parent.association().table(),
                child_task: (creator_task & !0xffff_ffff) | ((creator_task as u32 + 1) as u64),
                child_start: creator_start + 1,
                child_table: parent.association().table(),
                parent_task: creator_task,
                parent_start: creator_start,
                kernel_flags: flags.bits(),
                shared_mm: 1,
                shared_files: 1,
                same_thread_group: 1,
                exit_signal: -1,
                requested_exit_signal: 0,
                ready: 1,
                pidfd_fd: -1,
                ..Default::default()
            };
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
        runtime.retain_native_child(&birth, pin.as_fd()).unwrap();
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
        Self {
            runtime,
            parent,
            child,
            _birth: birth,
            metadata,
            memory,
            _peer: peer,
        }
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
