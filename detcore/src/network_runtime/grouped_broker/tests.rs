use std::future::Future;
use std::os::fd::AsFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::process::Stdio;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use super::*;

fn intent() -> Intent {
    Intent::new("1a".repeat(16), 31).unwrap()
}
fn directory() -> (tempfile::TempDir, OwnedFd) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = std::fs::File::open(directory.path()).unwrap();
    (directory, file.into())
}
fn journal() -> (tempfile::TempDir, journal::Journal) {
    let (root, fd) = directory();
    let mut journal = journal::Journal::retain(fd, intent());
    journal
        .initialize(json!({"controlled":"journal-only; no native custody"}))
        .unwrap();
    (root, journal)
}
fn frame(role: u32, done: bool) -> Value {
    let i = intent();
    let line = i.command(role, 0).unwrap();
    json!({"schema":"hermit-grouped-journal-v1","nonce":i.nonce,"sequence":role*2-1+u32::from(done),
        "owner":{"incarnation":31,"phase":2,"verified_sites":(1u32<<(role-1))-1,
            "attempted_sites":(1u32<<role)-1,"event_id":0,"write_unknown":0,"pending_role":role,
            "pending_remove":0,"pending_bytes":line.len(),"group":i.group(),"event":i.event()},
        "write":{"role":role,"remove":0,"submitted":line.len(),"raw":if done {line.len()} else {0},
            "error":0,"started":u32::from(done),"completed":u32::from(done)},"line":hex(line.as_bytes())})
}
fn bytes(frame: &Value) -> Vec<u8> {
    journal::canonical(frame).unwrap()
}
fn pair() -> (wire::Channel, wire::Channel) {
    let mut raw = [-1; 2];
    assert_eq!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                raw.as_mut_ptr(),
            )
        },
        0
    );
    for fd in raw {
        let yes = 1i32;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_PASSCRED,
                    (&yes as *const i32).cast(),
                    4,
                )
            },
            0
        );
    }
    (
        wire::Channel::retain(unsafe { OwnedFd::from_raw_fd(raw[0]) }),
        wire::Channel::retain(unsafe { OwnedFd::from_raw_fd(raw[1]) }),
    )
}
fn self_peer() -> wire::Credentials {
    wire::Credentials {
        pid: unsafe { libc::getpid() },
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
    }
}

#[test]
fn exact_creation_requires_all34_durable_frames_and17_pairs() {
    let (_root, mut j) = journal();
    for role in 1..=17 {
        for done in [false, true] {
            assert!(j.complete().is_err());
            let body = bytes(&frame(role, done));
            let ack = j.receive(&body, Instant::now()).unwrap();
            assert!(!ack.native_failed);
            assert_eq!(
                serde_json::from_slice::<Value>(&ack.bytes).unwrap()["sequence"],
                role * 2 - 1 + u32::from(done)
            );
            assert_eq!(
                j.store.file_syncs,
                1 + (role * 2 - 1 + u32::from(done)) as usize
            );
            assert!(j.store.directory_synced);
            j.store.verify().unwrap();
        }
    }
    j.complete().unwrap();
    assert_eq!(j.pairs.len(), 17);
    assert_eq!(j.create_mask, 0x1ffff);
    assert_eq!(j.next, 35);
    assert_eq!(j.store.content.split(|b| *b == b'\n').count(), 36);
    assert!(j.receive(&bytes(&frame(17, true)), Instant::now()).is_err());
    assert!(j.complete().is_err());
}
#[test]
fn intent_mutations_refuse_before_write_and_cannot_be_repaired() {
    let original = frame(1, false);
    let mut mutations = Vec::new();
    for (field, value) in [
        ("nonce", json!("2b".repeat(16))),
        ("sequence", json!(2)),
        ("schema", json!("other")),
        ("extra", json!(0)),
    ] {
        let mut v = original.clone();
        v[field] = value;
        mutations.push(bytes(&v));
    }
    for (field, value) in [
        ("phase", json!(7)),
        ("verified_sites", json!(1)),
        ("attempted_sites", json!(0)),
        ("incarnation", json!(32)),
        ("event_id", json!(1)),
        ("pending_role", json!(2)),
        ("write_unknown", json!(1)),
        ("pending_bytes", json!(0)),
    ] {
        let mut v = original.clone();
        v["owner"][field] = value;
        mutations.push(bytes(&v));
    }
    for (field, value) in [
        ("role", json!(0)),
        ("role", json!(18)),
        ("started", json!(1)),
        ("raw", json!(-1)),
        ("error", json!(1)),
        ("submitted", json!(true)),
    ] {
        let mut v = original.clone();
        v["write"][field] = value;
        mutations.push(bytes(&v));
    }
    let mut duplicate = String::from_utf8(bytes(&original)).unwrap();
    duplicate.insert_str(1, "\"sequence\":1,");
    mutations.push(duplicate.into_bytes());
    let mut trailing = bytes(&original);
    trailing.push(b' ');
    mutations.push(trailing);
    mutations.push(vec![b'x'; 1537]);
    assert_eq!(mutations.len(), 21);
    for malformed in mutations {
        let (_root, mut j) = journal();
        let old = j.store.content.clone();
        let writes = j.store.writes.len();
        assert!(j.receive(&malformed, Instant::now()).is_err());
        assert_eq!(j.store.content, old);
        assert_eq!(j.store.writes.len(), writes);
        assert!(j.receive(&bytes(&original), Instant::now()).is_err());
        assert_eq!(j.store.content, old);
    }
}
#[test]
fn failed_native_outcome_is_retained_and_acknowledged_without_success() {
    for (returned, errno) in [(-1, libc::EFAULT), (0, 0), (1, 0)] {
        let (_root, mut j) = journal();
        j.receive(&bytes(&frame(1, false)), Instant::now()).unwrap();
        let mut value = frame(1, true);
        value["write"]["raw"] = json!(returned);
        value["write"]["error"] = json!(errno);
        let ack = j.receive(&bytes(&value), Instant::now()).unwrap();
        assert!(ack.native_failed);
        assert_eq!(
            serde_json::from_slice::<Value>(&ack.bytes).unwrap()["sequence"],
            2
        );
        assert_eq!(j.next, 2);
        assert!(j.pending.is_none());
        assert!(j.pairs.is_empty());
        assert_eq!(j.create_mask, 0);
        assert_eq!(j.failed_pair.as_ref().unwrap()["outcome"], value["write"]);
        assert_eq!(j.store.file_syncs, 3);
        j.store.verify().unwrap();
        assert!(j.receive(&bytes(&frame(2, false)), Instant::now()).is_err());
        assert!(j.complete().is_err());
    }
}
#[test]
fn outcome_changes_keep_original_pending_evidence() {
    for (field, value) in [
        ("error", json!(1)),
        ("raw", json!(-1)),
        ("submitted", json!(0)),
        ("role", json!(2)),
        ("started", json!(0)),
    ] {
        let (_root, mut j) = journal();
        j.receive(&bytes(&frame(1, false)), Instant::now()).unwrap();
        let before = j.store.content.clone();
        let pending = j.pending.clone();
        let mut bad = frame(1, true);
        bad["write"][field] = value;
        assert!(j.receive(&bytes(&bad), Instant::now()).is_err());
        assert_eq!(j.pending, pending);
        assert_eq!(j.store.content, before);
        assert!(j.receive(&bytes(&frame(1, true)), Instant::now()).is_err());
    }
}
#[test]
fn actual_journal_tamper_and_name_collision_are_sticky_without_clobber() {
    let (root, mut j) = journal();
    let original = j.store.content.clone();
    let other = std::fs::File::open(root.path()).unwrap();
    let mut collision = journal::Store::retain(other.into(), &intent());
    assert!(collision.initialize(json!({"would":"clobber"})).is_err());
    assert!(collision.file.is_none());
    assert_eq!(
        std::fs::read(
            root.path()
                .join(format!("grouped-{}.jsonl", intent().nonce))
        )
        .unwrap(),
        original
    );
    let fd = j.store.file.as_ref().unwrap().as_raw_fd();
    assert_eq!(unsafe { libc::pwrite(fd, b"x".as_ptr().cast(), 1, 0) }, 1);
    assert!(j.store.verify().is_err());
    assert!(j.store.append(json!({"repair":true})).is_err());
    assert_eq!(j.store.content, original);
    assert!(j.store.file.is_some());
}
#[test]
fn actual_scm_aliases_and_credentials_survive_wrong_shape_and_truncation() {
    let (mut sender, mut receiver) = pair();
    let a = tempfile::tempfile().unwrap();
    let b = tempfile::tempfile().unwrap();
    sender.send_once(b"owned", &[a.as_fd(), b.as_fd()]).unwrap();
    let n = receiver.receive(512).unwrap().unwrap();
    let packet = &receiver.packets[n];
    packet.exact(2, self_peer()).unwrap();
    assert!(packet.exact(1, self_peer()).is_err());
    assert_eq!(packet.rights.len(), 2);
    for (original, received) in [a.as_raw_fd(), b.as_raw_fd()]
        .into_iter()
        .zip(&packet.rights)
    {
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_kcmp,
                    libc::getpid(),
                    libc::getpid(),
                    0,
                    original,
                    received.as_raw_fd(),
                )
            },
            0
        );
    }
    let mut wrong = self_peer();
    wrong.pid += 1;
    assert!(packet.exact(2, wrong).is_err());
    sender
        .send_once(&vec![b'x'; 513], &[a.as_fd(), b.as_fd()])
        .unwrap();
    let n = receiver.receive(512).unwrap().unwrap();
    assert!(receiver.packets[n].flags & libc::MSG_TRUNC != 0);
    assert!(receiver.packets[n].exact(2, self_peer()).is_err());
    assert_eq!(receiver.packets[n].rights.len(), 2);
}
#[test]
fn real_empty_packet_is_distinct_from_socket_eof() {
    let (sender, mut receiver) = pair();
    assert_eq!(
        unsafe {
            libc::send(
                sender.fd.as_raw_fd(),
                b"".as_ptr().cast(),
                0,
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        },
        0
    );
    let n = receiver.receive(512).unwrap().unwrap();
    assert_eq!(receiver.packets[n].raw.returned, 0);
    assert_eq!(receiver.packets[n].credentials, [self_peer()]);
    assert!(receiver.packets[n].exact(0, self_peer()).is_err());
    drop(sender);
    let n = receiver.receive(512).unwrap().unwrap();
    assert_eq!(receiver.packets[n].raw.returned, 0);
    assert!(receiver.packets[n].credentials.is_empty());
    assert!(receiver.packets[n].rights.is_empty());
}
#[test]
fn operation_drop_retains_actual_child_rights_and_partially_initialized_journal() {
    let (_logs, logs) = directory();
    let (_journal, journal_dir) = directory();
    let (mut sender, receiver) = pair();
    let sent = tempfile::tempfile().unwrap();
    sender
        .send_once(b"retained-on-cancel", &[sent.as_fd()])
        .unwrap();
    let mut command = Command::new("/usr/bin/sleep");
    command
        .arg("20")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    let pid = child.id() as i32;
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    assert!(raw >= 0);
    let held = unsafe { OwnedFd::from_raw_fd(raw) };
    let (recovery, operations) = RecoveryOwner::retain(
        intent(),
        format!("hermit-accepted-{}.service", intent().nonce),
        Instant::now() + Duration::from_secs(5),
        receiver.fd,
        child,
        logs,
        journal_dir,
    );
    // Controlled partial initialization exercises real ownership, not creator
    // admission or tracefs authority. No native authority is manufactured.
    let mut future = Box::pin(async move {
        operations
            .step(|s| {
                s.launcher.initialize(s.directory.as_raw_fd())?;
                s.journal
                    .initialize(json!({"controlled":"operation cancellation"}))?;
                assert!(s.channel.receive(512)?.is_some());
                Ok(())
            })
            .unwrap();
        std::future::pending::<()>().await;
    });
    assert!(matches!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    let before = recovery.diagnostics().unwrap();
    assert_eq!(before["received_aliases"], 1);
    assert_eq!(before["journal_file_held"], true);
    assert_eq!(before["launcher_pidfd_held"], true);
    drop(future);
    assert_eq!(Arc::strong_count(&recovery.state), 1);
    assert_eq!(recovery.diagnostics().unwrap(), before);
    assert!(!owner::terminal(held.as_raw_fd()).unwrap());
    let mut state = recovery.state.lock().unwrap();
    let fd = state.channel.packets[0].rights[0].as_raw_fd();
    assert_eq!(
        unsafe {
            libc::syscall(
                libc::SYS_kcmp,
                libc::getpid(),
                libc::getpid(),
                0,
                sent.as_raw_fd(),
                fd,
            )
        },
        0
    );
    assert_eq!(
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                held.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        },
        0
    );
    let status = state.launcher.child.wait().unwrap();
    assert!(!status.success());
    drop(state);
    recovery.begin_release(Instant::now()).unwrap();
    assert!(recovery.begin_release(Instant::now()).is_err());
}

#[test]
fn creator_status_requires_every_original_kernel_authority_field() {
    let peer = self_peer();
    let pid = peer.pid;
    let caps = (1u64 << 12) | (1 << 19) | (1 << 24) | (1 << 38) | (1 << 39);
    let mut valid = format!(
        "Pid:\t{pid}\nTgid:\t{pid}\nUid:\t{0}\t{0}\t{0}\t{0}\nGid:\t{1}\t{1}\t{1}\t{1}\nTracerPid:\t0\nNoNewPrivs:\t1\n",
        peer.uid, peer.gid
    );
    for key in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        valid += &format!("{key}:\t{caps:016x}\n");
    }
    owner::validate_process_status(&valid, peer).unwrap();
    for key in [
        "Pid",
        "Tgid",
        "Uid",
        "Gid",
        "TracerPid",
        "NoNewPrivs",
        "CapInh",
        "CapPrm",
        "CapEff",
        "CapBnd",
        "CapAmb",
    ] {
        let line = valid
            .lines()
            .find(|s| s.starts_with(&format!("{key}:")))
            .unwrap()
            .to_owned()
            + "\n";
        assert!(
            owner::validate_process_status(&valid.replace(&line, ""), peer).is_err(),
            "missing {key}"
        );
        assert!(
            owner::validate_process_status(&(valid.clone() + &line), peer).is_err(),
            "duplicate {key}"
        );
        let wrong = if key.starts_with("Cap") {
            format!("{key}:\t0000000000000000\n")
        } else {
            format!("{key}:\t999999\n")
        };
        assert!(
            owner::validate_process_status(&valid.replace(&line, &wrong), peer).is_err(),
            "wrong {key}"
        );
    }
    let expanded = valid.replace(
        &format!("CapEff:\t{caps:016x}"),
        &format!("CapEff:\t{:016x}", caps | 1),
    );
    assert!(owner::validate_process_status(&expanded, peer).is_err());
}
#[test]
fn durable_ack_cutoff_refuses_actual_socket_send_after_file_sync() {
    let (_directory, mut journal) = journal();
    let (mut sender, mut receiver) = pair();
    let expired = Instant::now();
    journal
        .store
        .append(json!({"durability":"before expired ACK"}))
        .unwrap();
    assert!(journal.store.file_syncs >= 2);
    assert!(send_after_durability(&mut sender, expired, b"held-proof").is_err());
    assert!(sender.sends.is_empty());
    assert!(receiver.receive(512).unwrap().is_none());
    send_after_durability(
        &mut sender,
        Instant::now() + Duration::from_secs(5),
        b"held-proof",
    )
    .unwrap();
    let packet = receiver.receive(512).unwrap().unwrap();
    assert_eq!(receiver.packets[packet].bytes, b"held-proof");
    assert_eq!(sender.sends.len(), 1);
}

#[test]
fn original128_query_bound_accepts_last_slot_and_refuses129th_before_spawn() {
    query_admission(true, None, 0, None, 0).unwrap();
    query_admission(true, None, 127, None, 127).unwrap(); // admits original128th owner
    assert!(query_admission(true, None, 128, None, 0).is_err());
    assert!(query_admission(true, None, 127, None, 128).is_err());
    assert!(query_admission(true, None, usize::MAX, None, 0).is_err());
    assert!(query_admission(true, None, 0, None, usize::MAX).is_err());
    assert!(query_admission(false, None, 0, None, 0).is_err());
    assert!(query_admission(true, Some(0), 0, None, 0).is_err());
    assert!(query_admission(true, None, 0, Some(0), 0).is_err());
}
