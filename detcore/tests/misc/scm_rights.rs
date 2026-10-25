/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Descriptors received through `SCM_RIGHTS` control messages.
//!
//! Most tests send a descriptor the process keeps, so the received one names
//! an open file the receiving table models and joins it as an alias
//! (`Detcore::join_received_alias`): it keeps that file's blocking state and
//! an eventfd's kind, and `MSG_CMSG_CLOEXEC` marks it alone. The others receive from a child a
//! random device and a rendered procfs file, which no descriptor of the
//! receiving table names. Descriptors from another process in general are
//! covered by `tests/c/scm_rights_received_descriptors.c`.

use std::io;
use std::mem::size_of;
use std::mem::size_of_val;
use std::ptr;

use detcore::Config;
use detcore::Detcore;
use libc::c_int;
use reverie::ExitStatus;

const RUNS: usize = 3;

/// `recvmsg` takes a different Detcore path with and without virtual time.
fn configs() -> [(&'static str, Config); 2] {
    [
        (
            "virtual time",
            Config {
                sequentialize_threads: true,
                virtualize_time: true,
                ..Default::default()
            },
        ),
        (
            "host time",
            Config {
                sequentialize_threads: true,
                virtualize_time: false,
                virtualize_metadata: false,
                ..Default::default()
            },
        ),
    ]
}

/// Runs `guest` under each configuration and requires identical output.
fn run_deterministically(guest: fn()) {
    for (name, config) in configs() {
        let mut expected = None;
        for run in 1..=RUNS {
            let (output, _state) =
                detcore_testutils::test_fn_with_config::<Detcore, _>(guest, config.clone(), true)
                    .unwrap_or_else(|error| panic!("{name} run {run} failed: {error:#}"));
            assert_eq!(
                output.status,
                ExitStatus::Exited(0),
                "{name} run {run} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            match &expected {
                Some(expected) => {
                    assert_eq!(
                        &output.stdout, expected,
                        "{name} output diverged on run {run}"
                    )
                }
                None => expected = Some(output.stdout),
            }
        }
    }
}

fn check(result: c_int, what: &str) -> c_int {
    assert!(result >= 0, "{what}: {}", io::Error::last_os_error());
    result
}

fn pipe() -> (c_int, c_int) {
    let mut fds = [0; 2];
    check(unsafe { libc::pipe(fds.as_mut_ptr()) }, "pipe");
    (fds[0], fds[1])
}

fn socketpair(ty: c_int) -> (c_int, c_int) {
    let mut fds = [0; 2];
    check(
        unsafe { libc::socketpair(libc::AF_UNIX, ty, 0, fds.as_mut_ptr()) },
        "socketpair",
    );
    (fds[0], fds[1])
}

fn write_all(fd: c_int, bytes: &[u8]) {
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    assert_eq!(
        written,
        bytes.len() as isize,
        "write to {fd}: {}",
        io::Error::last_os_error()
    );
}

fn read_exact(fd: c_int, len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    let mut filled = 0;
    while filled < len {
        let read = unsafe { libc::read(fd, bytes[filled..].as_mut_ptr().cast(), len - filled) };
        assert!(
            read > 0,
            "read from {fd}: {read} ({})",
            io::Error::last_os_error()
        );
        filled += read as usize;
    }
    bytes
}

fn fcntl(fd: c_int, command: c_int) -> c_int {
    check(unsafe { libc::fcntl(fd, command) }, "fcntl")
}

/// Sends one byte carrying `fds` in a single `SCM_RIGHTS` message.
fn send_fds(socket: c_int, fds: &[c_int]) {
    let mut payload = *b"f";
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let data_len = size_of_val(fds) as u32;
    // `u64` storage gives the buffer the alignment `cmsghdr` needs.
    let mut control = vec![0u64; (unsafe { libc::CMSG_SPACE(data_len) } as usize).div_ceil(8)];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = unsafe { libc::CMSG_SPACE(data_len) } as _;
    // SAFETY: the control buffer has room for one message of `data_len` bytes.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(data_len) as _;
        ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(header).cast(), fds.len());
    }
    let sent = unsafe { libc::sendmsg(socket, &message, 0) };
    assert_eq!(sent, 1, "sendmsg: {}", io::Error::last_os_error());
}

/// Descriptors in the `SCM_RIGHTS` messages the kernel wrote into `message`.
fn received_fds(message: &libc::msghdr) -> Vec<c_int> {
    let mut fds = Vec::new();
    // SAFETY: walks the control messages the kernel wrote into `message`.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data_len = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(header).cast::<c_int>();
                fds.extend(
                    (0..data_len / size_of::<c_int>()).map(|i| data.add(i).read_unaligned()),
                );
            }
            header = libc::CMSG_NXTHDR(message, header);
        }
    }
    fds
}

/// Receives one byte and returns the descriptors that came with it.
fn recv_fds(socket: c_int, flags: c_int) -> Vec<c_int> {
    let mut payload = [0u8];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let mut control = [0u64; 16];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = size_of_val(&control) as _;
    let received = unsafe { libc::recvmsg(socket, &mut message, flags) };
    assert_eq!(received, 1, "recvmsg: {}", io::Error::last_os_error());
    received_fds(&message)
}

/// A pipe end sent within the process names the open file the sender holds:
/// it keeps that file's blocking state and reaches the same pipe.
fn received_pipe_end_shares_the_open_file() {
    let (read_end, write_end) = pipe();
    let (left, right) = socketpair(libc::SOCK_STREAM);
    send_fds(left, &[write_end]);
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert_ne!(received, write_end);

    assert_eq!(
        fcntl(received, libc::F_GETFL) & libc::O_NONBLOCK,
        0,
        "the sender's pipe end is blocking"
    );
    write_all(received, b"through the received end");
    assert_eq!(read_exact(read_end, 24), b"through the received end");
    println!("descriptor {received} names the open file of {write_end}");
}

/// Detcore keeps pipes nonblocking in the kernel. A blocking read through a
/// received end must still wait for the writer rather than fail with EAGAIN.
fn blocking_read_on_a_received_pipe_waits_for_the_writer() {
    let (read_end, write_end) = pipe();
    let (left, right) = socketpair(libc::SOCK_STREAM);
    send_fds(left, &[read_end]);
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };

    let writer = std::thread::spawn(move || {
        for _ in 0..3 {
            std::thread::yield_now();
        }
        write_all(write_end, b"late");
    });
    assert_eq!(read_exact(received, 4), b"late");
    writer.join().unwrap();
    println!("blocking read on {received} returned the writer's bytes");
}

/// A random device from another process is Detcore's random stream, as one
/// this process opened is: the same bytes in every run.
fn random_device_from_a_child_reads_the_deterministic_stream() {
    let (left, right) = socketpair(libc::SOCK_STREAM);
    let child = check(unsafe { libc::fork() }, "fork");
    if child == 0 {
        let urandom = check(
            unsafe { libc::open(c"/dev/urandom".as_ptr(), libc::O_RDONLY) },
            "open /dev/urandom",
        );
        send_fds(left, &[urandom]);
        unsafe { libc::_exit(0) };
    }

    let [urandom] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let bytes = read_exact(urandom, 16);
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    println!("received /dev/urandom read {bytes:02x?}");
}

/// Reads `fd` to its end.
fn read_to_end(fd: c_int) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 256];
    loop {
        let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        assert!(read >= 0, "read from {fd}: {}", io::Error::last_os_error());
        if read == 0 {
            return bytes;
        }
        bytes.extend_from_slice(&buffer[..read as usize]);
    }
}

/// A procfs file Detcore renders, opened by another process, reads what the
/// file reads (the child shares the parent's cgroup, so the parent's own open
/// reads the same bytes), not the empty placeholder that stands in for it.
fn rendered_procfs_file_from_a_child_reads_as_the_file() {
    let own = check(
        unsafe { libc::open(c"/proc/self/cgroup".as_ptr(), libc::O_RDONLY) },
        "open /proc/self/cgroup",
    );
    let expected = read_to_end(own);
    assert!(!expected.is_empty());
    let (left, right) = socketpair(libc::SOCK_STREAM);
    let (go_read, go_write) = pipe();
    let child = check(unsafe { libc::fork() }, "fork");
    if child == 0 {
        let cgroup = check(
            unsafe { libc::open(c"/proc/self/cgroup".as_ptr(), libc::O_RDONLY) },
            "open /proc/self/cgroup",
        );
        send_fds(left, &[cgroup]);
        // Live until the parent has read: a reaped task's file reads ESRCH.
        read_exact(go_read, 1);
        unsafe { libc::_exit(0) };
    }

    let [cgroup] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let received = read_to_end(cgroup);
    write_all(go_write, b"g");
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    assert_eq!(
        String::from_utf8_lossy(&received),
        String::from_utf8_lossy(&expected)
    );
    println!(
        "received /proc/<child>/cgroup reads {:?}",
        String::from_utf8_lossy(&received)
    );
}

/// `recvmmsg` registers the descriptors of every message it delivers.
fn recvmmsg_registers_every_message() {
    let (first_read, first_write) = pipe();
    let (second_read, second_write) = pipe();
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    send_fds(left, &[first_write]);
    send_fds(left, &[second_write]);

    let mut payloads = [[0u8; 1]; 2];
    let mut iovs = payloads.each_mut().map(|payload| libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    });
    let mut controls = [[0u64; 16]; 2];
    let mut messages: [libc::mmsghdr; 2] = unsafe { std::mem::zeroed() };
    for ((message, iov), control) in messages.iter_mut().zip(&mut iovs).zip(&mut controls) {
        message.msg_hdr.msg_iov = iov;
        message.msg_hdr.msg_iovlen = 1;
        message.msg_hdr.msg_control = control.as_mut_ptr().cast();
        message.msg_hdr.msg_controllen = size_of_val(control) as _;
    }
    let delivered = unsafe { libc::recvmmsg(right, messages.as_mut_ptr(), 2, 0, ptr::null_mut()) };
    assert_eq!(delivered, 2, "recvmmsg: {}", io::Error::last_os_error());

    let received: Vec<c_int> = messages
        .iter()
        .flat_map(|message| received_fds(&message.msg_hdr))
        .collect();
    let [first, second] = received[..] else {
        panic!("expected one descriptor per message, got {received:?}");
    };
    for fd in [first, second] {
        assert_eq!(
            fcntl(fd, libc::F_GETFL) & libc::O_NONBLOCK,
            0,
            "the sender's pipe ends are blocking"
        );
    }
    write_all(first, b"one");
    write_all(second, b"two");
    assert_eq!(read_exact(first_read, 3), b"one");
    assert_eq!(read_exact(second_read, 3), b"two");
    println!("recvmmsg delivered {first} and {second}");
}

/// `MSG_CMSG_CLOEXEC` sets close-on-exec on the received descriptor only.
fn msg_cmsg_cloexec_marks_received_descriptors() {
    let (_read_end, write_end) = pipe();
    let (left, right) = socketpair(libc::SOCK_STREAM);
    send_fds(left, &[write_end]);
    send_fds(left, &[write_end]);

    let [plain] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let [cloexec] = recv_fds(right, libc::MSG_CMSG_CLOEXEC)[..] else {
        panic!("expected one descriptor");
    };
    assert_eq!(fcntl(plain, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
    assert_eq!(
        fcntl(cloexec, libc::F_GETFD) & libc::FD_CLOEXEC,
        libc::FD_CLOEXEC
    );
    assert_eq!(fcntl(write_end, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
    write_all(plain, b"p");
    write_all(cloexec, b"c");
    println!("{cloexec} is close-on-exec, {plain} is not");
}

fn eventfd(initial: u32, flags: c_int) -> c_int {
    check(unsafe { libc::eventfd(initial, flags) }, "eventfd")
}

/// One `read` of an eventfd's eight bytes.
fn read_exact_once(fd: c_int) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    assert_eq!(read, 8, "read from {fd}: {}", io::Error::last_os_error());
    bytes
}

fn eventfd_write(fd: c_int, value: u64) {
    write_all(fd, &value.to_ne_bytes());
}

/// An eventfd sent within the process is the sender's eventfd: a count
/// written through either descriptor is read through the other, and an
/// `EFD_SEMAPHORE` eventfd hands out one unit per read through both.
fn received_eventfd_shares_the_counter() {
    let (left, right) = socketpair(libc::SOCK_STREAM);
    let original = eventfd(0, 0);
    let semaphore = eventfd(2, libc::EFD_SEMAPHORE);
    send_fds(left, &[original, semaphore]);
    let [received, received_semaphore] = recv_fds(right, 0)[..] else {
        panic!("expected two descriptors");
    };
    eventfd_write(received, 5);
    let through_original = u64::from_ne_bytes(read_exact_once(original));
    eventfd_write(original, 7);
    let through_received = u64::from_ne_bytes(read_exact_once(received));
    let units = [
        u64::from_ne_bytes(read_exact_once(received_semaphore)),
        u64::from_ne_bytes(read_exact_once(semaphore)),
    ];
    assert_eq!((through_original, through_received), (5, 7));
    assert_eq!(units, [1, 1]);
    println!("eventfd {received} shares {original}'s counter, semaphore units {units:?}");
}

#[test]
fn scm_rights_pipe_end_shares_the_open_file() {
    run_deterministically(received_pipe_end_shares_the_open_file);
}

#[test]
fn scm_rights_eventfd_shares_the_counter() {
    run_deterministically(received_eventfd_shares_the_counter);
}

#[test]
fn scm_rights_blocking_read_waits_for_the_writer() {
    run_deterministically(blocking_read_on_a_received_pipe_waits_for_the_writer);
}

#[test]
fn scm_rights_random_device_from_a_child_process() {
    run_deterministically(random_device_from_a_child_reads_the_deterministic_stream);
}

#[test]
fn scm_rights_rendered_procfs_file_from_a_child_process() {
    run_deterministically(rendered_procfs_file_from_a_child_reads_as_the_file);
}

#[test]
fn scm_rights_through_recvmmsg() {
    run_deterministically(recvmmsg_registers_every_message);
}

#[test]
fn scm_rights_with_msg_cmsg_cloexec() {
    run_deterministically(msg_cmsg_cloexec_marks_received_descriptors);
}
