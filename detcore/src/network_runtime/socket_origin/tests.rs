//! Logical origin has no host-option or finite-release authority. The actual
//! installation predicate and same original Normal/call checks issue it.
use super::*;

fn authority_fixture() -> (
    SocketBirthAuthority,
    NetworkStreamOwner,
    Admission,
    Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
) {
    use crate::network_replay::NetworkFdMutationKind;
    use crate::network_replay::NetworkReplayEngine;
    use crate::network_replay::original_connect::Arguments;
    use crate::network_replay::original_connect::Kind;
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (root, metadata, _memory, claim) = crate::network_runtime::controlled_foreground_root(raw);
    let owner = root.owner();
    let mut scheduler = crate::scheduler::Scheduler::new(&crate::Config::default());
    scheduler.controlled_foreground_store_grant(&root);
    let mut engine = NetworkReplayEngine::record_shared_mm_attempts(
        chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
    );
    engine.fd_table_fixture_enable();
    engine
        .register_initial_census(root.association(), &claim, owner.thread)
        .unwrap();
    engine
        .associate_fd_metadata(owner, &metadata, &metadata.lock().unwrap())
        .unwrap();
    let crate::network_replay::NetworkFdMutationBegin::Admitted(mutation) = engine
        .begin_fd_mutation(owner, root.files(), NetworkFdMutationKind::Socket)
        .unwrap()
    else {
        panic!("original Socket");
    };
    engine
        .submit_fd_mutation(owner, mutation.publication.permit)
        .unwrap();
    let admission = engine
        .begin_original_socket(
            owner,
            Arguments {
                kind: Kind::Socket,
                operation: crate::resources::ExternalOpId::new(owner.thread, 10),
                files: root.files(),
                binding: None,
                fd: libc::AF_INET,
                address: libc::SOCK_STREAM as u64,
                length: 0,
                original_count: 0,
            },
            *mutation,
        )
        .unwrap();
    let grant = scheduler
        .foreground_native_observation(owner, &root)
        .unwrap();
    let authority = SocketBirthAuthority::from_original(root.clone(), &grant, &admission).unwrap();
    (authority, owner, admission, metadata)
}

#[test]
fn socket_origin_requires_the_same_original_owner_and_call() {
    let (authority, owner, admission, _metadata) = authority_fixture();
    let plan = Plan::capture(authority);
    let origin = plan.complete_origin(owner, &admission).unwrap();
    assert!(origin.validates(owner, admission.call));
    assert!(origin.matches_initial_root(plan.authority.root()));
    let mut wrong = admission.clone();
    wrong.arguments.original_count += 1;
    assert!(plan.complete_origin(owner, &wrong).is_err());
    let mut wrong_owner = owner;
    wrong_owner.mm = owner.mm.for_exec(owner.thread);
    assert!(plan.complete_origin(wrong_owner, &admission).is_err());
    assert!(!origin.validates(wrong_owner, admission.call));
}

#[test]
fn socket_origin_requires_original_socket_installation() {
    use crate::network_replay::NetworkFdPublicationPermit;
    use crate::network_replay::NetworkStreamLeaseId;
    use crate::network_runtime::original_installation::installation_fixture;
    use crate::network_runtime::original_installation::installation_fixture_for_root;
    let (authority, owner, admission, _metadata) = authority_fixture();
    let root = authority.root().clone();
    let plan = Plan::capture(authority);
    let permit = NetworkFdPublicationPermit {
        files: root.files(),
        owner,
        lease: NetworkStreamLeaseId::controlled_fixture(19),
    };
    let make = |source| installation_fixture_for_root(&root, permit, source, 41, 3, false);
    let installation = make(Source::Socket(admission.call));
    assert!(plan.complete(owner, &admission, &installation).is_ok());
    let foreign_identity = installation_fixture(
        owner,
        root.metadata().unwrap(),
        permit,
        Source::Socket(admission.call),
        41,
        3,
        false,
    );
    assert!(plan.complete(owner, &admission, &foreign_identity).is_err());
    let unrelated = make(Source::Openat(admission.call));
    assert!(plan.complete(owner, &admission, &unrelated).is_err());
    let wrong_call = make(Source::Socket(
        crate::network_replay::NetworkStreamCallId::controlled_fixture(999),
    ));
    assert!(plan.complete(owner, &admission, &wrong_call).is_err());
}
