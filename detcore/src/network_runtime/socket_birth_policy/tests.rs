//! Explicit controlled host-query receipts exercise the production predicate;
//! these controls do not claim to inspect a live host BPF policy.
use super::*;

fn retained_snapshot(effective: bool) -> Snapshot {
    let query = |attach| Query {
        attach,
        flags: BPF_F_QUERY_EFFECTIVE,
        returned: 0,
        errno: None,
        count: 0,
        attach_flags: 0,
        revision: 0,
    };
    Snapshot {
        creator: Some(Creator {
            pidfd: Node {
                device: 1,
                inode: 2,
            },
            pid: 31,
            tgid: 31,
            cgroup: 71,
        }),
        namespace: Some(Node {
            device: 3,
            inode: CGROUP_NS_INIT_INO,
        }),
        membership: Some("/fixture/guest".into()),
        directory: Some(Directory {
            node: Node {
                device: 4,
                inode: 71,
            },
            mount: 19,
        }),
        queries: effective.then(|| [query(GETSOCKOPT), query(SETSOCKOPT)]),
        releases: vec![
            Release {
                returned: 0,
                errno: None
            };
            3
        ],
        opened: 3,
        error: None,
    }
}

#[test]
fn socket_birth_policy_effective_queries_require_both_exact_zero_receipts() {
    let before = retained_snapshot(false);
    let after = retained_snapshot(true);
    assert!(before.same_birth(&after));
    for which in 0..2 {
        for mutation in 0..7 {
            let mut changed = after.clone();
            let q = &mut changed.queries.as_mut().unwrap()[which];
            match mutation {
                0 => q.count = 1,
                1 => {
                    q.returned = -1;
                    q.errno = Some(libc::EPERM);
                }
                2 => {
                    q.returned = -1;
                    q.errno = Some(libc::EINVAL);
                }
                3 => q.flags = 0,
                4 => q.attach = 3,
                5 => q.attach_flags = 1,
                6 => q.revision = 1,
                _ => unreachable!(),
            }
            assert!(
                !changed.admits_getter(),
                "query {which} mutation {mutation}"
            );
            assert!(!before.same_birth(&changed));
        }
    }
}

#[test]
fn socket_birth_policy_identity_and_positive_releases_are_independent() {
    let before = retained_snapshot(false);
    for mutation in 0..11 {
        let mut after = retained_snapshot(true);
        match mutation {
            0 => after.creator.as_mut().unwrap().pidfd.inode += 1,
            1 => after.creator.as_mut().unwrap().pid += 1,
            2 => after.creator.as_mut().unwrap().cgroup += 1,
            3 => after.namespace.as_mut().unwrap().inode += 1,
            4 => after.directory.as_mut().unwrap().node.inode += 1,
            5 => after.directory.as_mut().unwrap().mount = 0,
            6 => after.membership = Some("/different".into()),
            7 => {
                after.releases.pop();
            }
            8 => after.releases[0].returned = -1,
            9 => after.releases[1].errno = Some(libc::EINTR),
            10 => after.error = Some("controlled incomplete observation".into()),
            _ => unreachable!(),
        }
        assert!(
            !before.same_birth(&after),
            "identity/release mutation {mutation}"
        );
    }
}

#[test]
fn socket_birth_policy_rejects_unanchored_and_ambiguous_membership() {
    assert_eq!(membership("0::/guest/a\n").unwrap(), "/guest/a");
    for text in [
        "1:cpu:/guest\n",
        "0::/guest\n1:cpu:/guest\n",
        "0::/../guest\n",
        "0::/a/./b\n",
        "0::relative\n",
        "0::/bad\\040name\n",
        "0::/guest",
    ] {
        assert!(membership(text).is_err(), "{text:?}");
    }
    let good = "19 1 0:41 / /sys/fs/cgroup rw - cgroup2 cgroup rw\n";
    assert_eq!(mount(good).unwrap(), (19, "/sys/fs/cgroup".into()));
    assert!(mount(&format!("{good}{good}")).is_err());
    assert!(mount("19 1 0:41 /delegated /sys/fs/cgroup rw - cgroup2 cgroup rw\n").is_err());
    assert!(mount("19 1 0:41 / /sys/fs/cgroup rw - cgroup cgroup rw\n").is_err());
}

#[test]
fn socket_birth_policy_disabled_linger_requires_actual_sized_success() {
    let good = Observation {
        policy: retained_snapshot(true),
        returned: Some(0),
        errno: None,
        length: 8,
        value: [0, 5],
    };
    assert_eq!(good.disabled_linger(), Some((0, 5)));
    for mutation in 0..6 {
        let mut changed = good.clone();
        match mutation {
            0 => changed.returned = None,
            1 => changed.returned = Some(-1),
            2 => changed.errno = Some(libc::EIO),
            3 => changed.length = 4,
            4 => changed.value[0] = 1,
            5 => changed.policy.queries.as_mut().unwrap()[0].count = 1,
            _ => unreachable!(),
        }
        assert_eq!(changed.disabled_linger(), None);
    }
}

pub(super) fn complete_controlled_birth(
    authority: SocketBirthAuthority,
    owner: NetworkStreamOwner,
    admission: &Admission,
) -> io::Result<Arc<Completed>> {
    let mut before = retained_snapshot(false);
    let mut after = retained_snapshot(true);
    let pid = authority.root.thread() as u32;
    for snapshot in [&mut before, &mut after] {
        let creator = snapshot.creator.as_mut().unwrap();
        creator.pid = pid;
        creator.tgid = pid;
    }
    let observation = Observation {
        policy: after.clone(),
        returned: Some(0),
        errno: None,
        length: 8,
        value: [0, 0],
    };
    Plan { authority, before }
        .complete(owner, admission, &after, &observation)?
        .ok_or_else(|| io::Error::other("controlled birth was not eligible"))
}

fn authority_fixture() -> (
    SocketBirthAuthority,
    NetworkStreamOwner,
    Admission,
    crate::network_replay::NetworkReplayEngine,
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
    (authority, owner, admission, engine)
}

#[test]
fn socket_birth_policy_completion_refuses_wrong_call_and_unreleased_preparation() {
    let (authority, owner, admission, _engine) = authority_fixture();
    let mut before = retained_snapshot(false);
    before.creator.as_mut().unwrap().pid = authority.root.thread() as u32;
    before.creator.as_mut().unwrap().tgid = authority.root.thread() as u32;
    let mut after = before.clone();
    after.queries = retained_snapshot(true).queries;
    let observed = Observation {
        policy: after.clone(),
        returned: Some(0),
        errno: None,
        length: 8,
        value: [0, 0],
    };
    let good = Plan {
        authority: authority.clone(),
        before,
    };
    assert!(good.released());
    assert!(
        good.complete(owner, &admission, &after, &observed)
            .unwrap()
            .is_some()
    );
    let mut wrong = admission.clone();
    wrong.arguments.original_count += 1;
    assert!(good.complete(owner, &wrong, &after, &observed).is_err());
    let mut wrong_owner = owner;
    wrong_owner.mm = owner.mm.for_exec(owner.thread);
    assert!(
        good.complete(wrong_owner, &admission, &after, &observed)
            .is_err()
    );
    for mutation in 0..3 {
        let mut plan = good.clone();
        match mutation {
            0 => {
                plan.before.releases.pop();
            }
            1 => plan.before.releases[0].returned = -1,
            2 => plan.before.releases[0].errno = Some(libc::EINTR),
            _ => unreachable!(),
        }
        assert!(!plan.released(), "pre-submission gate mutation {mutation}");
        assert!(plan.complete(owner, &admission, &after, &observed).is_err());
    }
    let mut unsupported = good;
    unsupported.before.error = Some("controlled unsupported mount".into());
    assert!(unsupported.released());
    assert!(
        unsupported
            .complete(owner, &admission, &after, &observed)
            .unwrap()
            .is_none()
    );
}

#[test]
fn socket_birth_policy_capture_keeps_raw_outcomes_after_metadata_error() {
    let (effect, capture) = super::super::installation_observation::fixture();
    let observed = Observation {
        policy: retained_snapshot(true),
        returned: Some(0),
        errno: None,
        length: 8,
        value: [0, 0],
    };
    let mut successful = capture.clone();
    successful.finite_close = Some(observed.clone());
    assert_eq!(
        successful.checked(&effect).unwrap().unwrap().finite_close,
        Some(observed.clone())
    );
    let mut failed = successful.clone();
    failed.metadata = None;
    failed.error = Some("controlled later socket getter failed".into());
    assert!(failed.checked(&effect).is_err());
    assert_eq!(failed.finite_close, Some(observed.clone()));
    let serialized = serde_json::to_vec(&failed).unwrap();
    let reread: super::super::installation_observation::Capture =
        serde_json::from_slice(&serialized).unwrap();
    assert_eq!(reread, failed);
    for non_tcp in [false, true] {
        let mut failed_release = successful.clone();
        failed_release
            .finite_close
            .as_mut()
            .unwrap()
            .policy
            .releases[1]
            .returned = -1;
        failed_release
            .finite_close
            .as_mut()
            .unwrap()
            .policy
            .releases[1]
            .errno = Some(libc::EINTR);
        if non_tcp {
            failed_release.metadata.as_mut().unwrap().tcp = None;
        }
        assert_eq!(
            failed_release.checked(&effect).unwrap_err().to_string(),
            "Socket birth policy descriptor release remains unresolved"
        );
        assert_eq!(
            failed_release
                .finite_close
                .as_ref()
                .unwrap()
                .policy
                .releases[1]
                .errno,
            Some(libc::EINTR)
        );
    }
    let debug = format!("{successful:?}");
    assert!(!debug.contains("/fixture/guest"));
    assert!(!debug.contains("cgroup: 71"));
}

#[tokio::test]
async fn socket_birth_policy_failed_close_remains_in_actual_native_registry() {
    let (authority, owner, admission, engine) = authority_fixture();
    let mut before = retained_snapshot(false);
    before.releases[0].returned = -1;
    before.releases[0].errno = Some(libc::EINTR);
    let plan = Plan { authority, before };
    let publication = crate::network_runtime::NativeCaptureRecovery::new(
        Arc::new(std::sync::Mutex::new(engine)),
        Arc::new(tokio::sync::Notify::new()),
        |_| {},
    );
    let mut calls = super::super::native_peer::Calls::default();
    calls
        .capture_original(
            owner,
            admission.clone(),
            None,
            tokio::runtime::Handle::current(),
            publication,
        )
        .unwrap();
    calls.original(owner, admission.call).unwrap().socket_birth = Some(plan);
    calls
        .abandon_original_before_provider(owner, admission.call)
        .unwrap();
    let release = calls
        .prepare_release(owner, admission.call)
        .unwrap()
        .perform();
    calls
        .retain_release(owner, admission.call, release)
        .unwrap();
    assert_eq!(
        calls
            .finish_release(owner, admission.call)
            .unwrap_err()
            .to_string(),
        "Socket birth pre-entry release is unresolved"
    );
    let retained = calls.original(owner, admission.call).unwrap();
    assert!(retained.prepare_request.is_none());
    assert!(retained.prepared.is_none());
    assert!(retained.selection.is_none());
    assert_eq!(
        retained.socket_birth.as_ref().unwrap().before.releases[0].errno,
        Some(libc::EINTR)
    );
    assert!(calls.settled().is_err());
}

#[test]
fn socket_birth_policy_cross_namespace_coordinates_keep_stable_objects_exact() {
    let before = retained_snapshot(false);
    let mut after = retained_snapshot(true);
    after.creator.as_mut().unwrap().pid += 1000;
    after.creator.as_mut().unwrap().tgid += 1000;
    after.directory.as_mut().unwrap().mount += 1000;
    assert!(before.same_birth(&after));
    for mutation in 0..9 {
        let mut changed = after.clone();
        match mutation {
            0 => changed.creator.as_mut().unwrap().pidfd.device += 1,
            1 => changed.creator.as_mut().unwrap().pidfd.inode += 1,
            2 => changed.creator.as_mut().unwrap().cgroup += 1,
            3 => changed.namespace.as_mut().unwrap().device += 1,
            4 => changed.namespace.as_mut().unwrap().inode += 1,
            5 => changed.directory.as_mut().unwrap().node.device += 1,
            6 => changed.directory.as_mut().unwrap().node.inode += 1,
            7 => changed.membership = Some("/another-cgroup".into()),
            8 => changed.creator.as_mut().unwrap().tgid += 1,
            _ => unreachable!(),
        }
        assert!(
            !before.same_birth(&changed),
            "stable identity mutation {mutation}"
        );
    }
}
