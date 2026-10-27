/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Descriptors received through `SCM_RIGHTS` control messages.
//!
//! Every message goes through a `socketpair`, whose sends Detcore queues with
//! the open file each descriptor names (`detcore::syscalls::scm_rights`), so
//! a received descriptor whose sender still holds it joins that open file as
//! an alias: it keeps that file's blocking state and an eventfd's kind, and
//! `MSG_CMSG_CLOEXEC` marks it alone. Most tests send a descriptor the process
//! keeps; one receives a pipe end from a child that keeps it, which no
//! descriptor of the receiving table names, and one follows the queue through
//! a peek, a truncated receive and a discarded message. Others receive from a
//! child a random device and a rendered procfs file. Descriptors from another
//! process in general are covered by
//! `tests/c/scm_rights_received_descriptors.c`.
//!
//! Each run starts in a fresh PID namespace (`run_deterministically`).

use std::io;
use std::mem::size_of;
use std::mem::size_of_val;
use std::ptr;

use detcore::Config;
use detcore_testutils::det_test_fn_with_config_repetitions;
use libc::c_int;
use reverie::ExitStatus;

const RUNS: u64 = 3;

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

/// Runs `guest` `RUNS` times under each configuration and requires every
/// run to exit 0 with the same stdout, stderr and schedule
/// (`det_test_fn_with_config_repetitions`).
///
/// Each run starts in a fresh PID namespace, as a `hermit run` container
/// does, so its guest's thread ids repeat. Detcore names some of an open
/// file's state by a thread id: a random device's stream by the task that
/// opened it or, for one it received, by the receiving table's owner
/// (`RandomDeviceStream`). In the test process's own PID namespace those ids
/// are host PIDs, new in every run, and a random device received from a child
/// read different bytes each time.
fn run_deterministically(guest: fn()) {
    for (name, config) in configs() {
        det_test_fn_with_config_repetitions(RUNS, true, guest, config, |output, _state| {
            assert_eq!(
                output.status,
                ExitStatus::Exited(0),
                "{name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            // Every guest prints what it found. An empty stdout would compare
            // equal in every run, whatever the guest read.
            assert!(
                !output.stdout.is_empty(),
                "{name}: the guest printed nothing"
            );
        });
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

/// Sends one byte carrying `fds` in a single `SCM_RIGHTS` message. It
/// allocates nothing, so a child forked from the test process can call it.
fn send_fds(socket: c_int, fds: &[c_int]) {
    let mut payload = *b"f";
    assert_eq!(send_fds_with(socket, fds, &mut payload), 1);
}

/// Sends `payload` carrying `fds` in a single `SCM_RIGHTS` message and
/// returns how many of its bytes Linux took.
fn send_fds_with(socket: c_int, fds: &[c_int], payload: &mut [u8]) -> usize {
    send_fds_with_name(socket, fds, payload, None)
}

/// As [`send_fds`], to the socket `address` names (`msg_name`).
fn send_fds_to(socket: c_int, fds: &[c_int], address: &(libc::sockaddr_un, u32)) {
    let mut payload = *b"f";
    assert_eq!(
        send_fds_with_name(socket, fds, &mut payload, Some(address)),
        1
    );
}

/// As [`send_fds_with`], naming the destination `name` where given.
fn send_fds_with_name(
    socket: c_int,
    fds: &[c_int],
    payload: &mut [u8],
    name: Option<&(libc::sockaddr_un, u32)>,
) -> usize {
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let data_len = size_of_val(fds) as u32;
    // `u64` storage gives the buffer the alignment `cmsghdr` needs: room for
    // the header and 28 descriptors.
    let mut control = [0u64; 16];
    assert!(unsafe { libc::CMSG_SPACE(data_len) } as usize <= size_of_val(&control));
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = unsafe { libc::CMSG_SPACE(data_len) } as _;
    if let Some((address, length)) = name {
        message.msg_name = (&raw const *address).cast_mut().cast();
        message.msg_namelen = *length;
    }
    // SAFETY: the control buffer has room for one message of `data_len` bytes.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(data_len) as _;
        ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(header).cast(), fds.len());
    }
    let sent = unsafe { libc::sendmsg(socket, &message, 0) };
    assert!(sent > 0, "sendmsg: {}", io::Error::last_os_error());
    sent as usize
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
    recv_fds_into(socket, flags, size_of::<[u64; 16]>()).0
}

/// Receives one byte into a control buffer of `room` bytes (none for 0), and
/// returns the descriptors that came with it and whether Linux set
/// `MSG_CTRUNC`.
fn recv_fds_into(socket: c_int, flags: c_int, room: usize) -> (Vec<c_int>, bool) {
    let mut payload = [0u8];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let mut control = [0u64; 16];
    assert!(room <= size_of_val(&control));
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    if room > 0 {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = room as _;
    }
    let received = unsafe { libc::recvmsg(socket, &mut message, flags) };
    assert_eq!(received, 1, "recvmsg: {}", io::Error::last_os_error());
    (
        received_fds(&message),
        message.msg_flags & libc::MSG_CTRUNC != 0,
    )
}

/// Whether `fd` is blocking, as its open file's status flags say.
fn blocking(fd: c_int) -> bool {
    fcntl(fd, libc::F_GETFL) & libc::O_NONBLOCK == 0
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
/// The writer is another process, not a thread: each guest descends, by
/// fork, from the multithreaded test process, and a Rust thread started there
/// can wait forever on a lock of the standard library's that another thread
/// of the test process held at the fork. A forked writer takes no such lock.
fn blocking_read_on_a_received_pipe_waits_for_the_writer() {
    let (read_end, write_end) = pipe();
    let (left, right) = socketpair(libc::SOCK_STREAM);
    send_fds(left, &[read_end]);
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };

    let writer = check(unsafe { libc::fork() }, "fork");
    if writer == 0 {
        // Only the parent reads.
        unsafe {
            libc::close(read_end);
            libc::close(received);
        }
        for _ in 0..3 {
            unsafe { libc::sched_yield() };
        }
        write_all(write_end, b"late");
        unsafe { libc::_exit(0) };
    }
    // The writer holds the only write end: if it fails, the read ends at the
    // end of file and the test fails, instead of waiting forever.
    check(unsafe { libc::close(write_end) }, "close");
    assert_eq!(read_exact(received, 4), b"late");
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(writer, &mut status, 0) }, writer);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
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

/// A pipe end received from another process that keeps it names the open
/// file the sender holds, which no descriptor of the receiving table names:
/// it keeps that file's blocking state and reaches the same pipe. Detcore
/// keeps the pipe physically nonblocking, so a description of its own, made
/// from the kernel's flags, would say nonblocking. The child calls only
/// functions that allocate nothing (see
/// `blocking_read_on_a_received_pipe_waits_for_the_writer`).
fn pipe_end_from_a_process_that_keeps_it_shares_the_open_file() {
    let (left, right) = socketpair(libc::SOCK_STREAM);
    let sender = check(unsafe { libc::fork() }, "fork");
    if sender == 0 {
        unsafe { libc::close(right) };
        let (read_end, write_end) = pipe();
        send_fds(left, &[write_end]);
        // Keep the pipe until the parent has written through its end.
        let mut byte = [0u8];
        let read = unsafe { libc::read(left, byte.as_mut_ptr().cast(), 1) };
        let mut text = [0u8; 4];
        let got = unsafe { libc::read(read_end, text.as_mut_ptr().cast(), 4) };
        let read_back = read == 1 && got == 4 && &text == b"from";
        unsafe { libc::_exit(if read_back { 0 } else { 1 }) };
    }
    unsafe { libc::close(left) };
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert!(blocking(received), "the sender's pipe end is blocking");
    write_all(received, b"from");
    write_all(right, b"k");
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(sender, &mut status, 0) }, sender);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "the sender did not read what was written through the received end"
    );
    println!("descriptor {received} names the open file its sender holds");
}

/// An abstract `AF_UNIX` address for `name`, which starts with its NUL:
/// nothing on the file system, freed when the run ends.
fn abstract_address(name: &[u8]) -> (libc::sockaddr_un, u32) {
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(name) {
        *slot = *byte as libc::c_char;
    }
    let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + name.len()) as u32;
    (address, length)
}

fn unix_stream_socket() -> c_int {
    check(
        unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) },
        "socket",
    )
}

/// A connection a child process makes to its parent's listener carries open
/// files as one within a process does: the child's pipe end, sent while the
/// child keeps it, names the child's open file in the parent. The bind and
/// the accept run in the parent and the connect in the child, so they meet
/// only through the connections the run's tasks share. The child calls only
/// functions that allocate nothing.
fn descriptors_over_a_connection_from_a_child_share_their_open_files() {
    let (address, length) = abstract_address(b"\0hermit-detcore-scm-rights-child-connection");
    let at = (&raw const address).cast::<libc::sockaddr>();
    let listener = unix_stream_socket();
    check(unsafe { libc::bind(listener, at, length) }, "bind");
    check(unsafe { libc::listen(listener, 1) }, "listen");
    let child = check(unsafe { libc::fork() }, "fork");
    if child == 0 {
        unsafe { libc::close(listener) };
        let client = unix_stream_socket();
        if unsafe { libc::connect(client, at, length) } != 0 {
            unsafe { libc::_exit(2) };
        }
        let (read_end, write_end) = pipe();
        send_fds(client, &[write_end]);
        // Keep the pipe until the parent has written through its end.
        let mut byte = [0u8];
        let read = unsafe { libc::read(client, byte.as_mut_ptr().cast(), 1) };
        let mut text = [0u8; 4];
        let got = unsafe { libc::read(read_end, text.as_mut_ptr().cast(), 4) };
        let read_back = read == 1 && got == 4 && &text == b"kept";
        unsafe { libc::_exit(if read_back { 0 } else { 1 }) };
    }
    let server = check(
        unsafe { libc::accept(listener, ptr::null_mut(), ptr::null_mut()) },
        "accept",
    );
    let [received] = recv_fds(server, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert!(blocking(received), "the child's pipe end is blocking");
    write_all(received, b"kept");
    write_all(server, b"k");
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "the child did not read what was written through the received end"
    );
    println!("descriptor {received} from a child's connection names the child's open file");
}

/// A connection the process made to its own listener carries open files as
/// a `socketpair` does: a pipe write end the connecting socket sends names
/// the sender's blocking open file, and an eventfd the accepted socket sends
/// back shares the sender's counter. Detcore pairs the accepted connection
/// with the socket whose `connect` it took.
fn descriptors_over_a_connection_to_an_own_listener_share_their_open_files() {
    let (address, length) = abstract_address(b"\0hermit-detcore-scm-rights-connected");
    let listener = unix_stream_socket();
    let at = (&raw const address).cast::<libc::sockaddr>();
    check(unsafe { libc::bind(listener, at, length) }, "bind");
    check(unsafe { libc::listen(listener, 1) }, "listen");
    let client = unix_stream_socket();
    check(unsafe { libc::connect(client, at, length) }, "connect");
    let server = check(
        unsafe { libc::accept(listener, ptr::null_mut(), ptr::null_mut()) },
        "accept",
    );

    let (read_end, write_end) = pipe();
    send_fds(client, &[write_end]);
    let [received_end] = recv_fds(server, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert!(blocking(received_end), "the sender's pipe end is blocking");
    write_all(received_end, b"over");
    assert_eq!(read_exact(read_end, 4), b"over");

    let counter = eventfd(0, 0);
    send_fds(server, &[counter]);
    let [received_counter] = recv_fds(client, 0)[..] else {
        panic!("expected one descriptor");
    };
    eventfd_write(received_counter, 9);
    let count = u64::from_ne_bytes(read_exact_once(counter));
    assert_eq!(count, 9);
    println!(
        "pipe end {received_end} and eventfd {received_counter} name their senders' open files"
    );
}

/// The status flags of descriptor `received`, an eventfd, as a line to
/// print: Linux gives the sender's blocking open file, and Detcore, where it
/// cannot tell which eventfd it is, refuses the descriptor (`F_GETFL` fails
/// with EBADF, as main fails it); what it must never do is give another
/// sender's nonblocking one.
fn blocking_eventfd_status(received: c_int, what: &str) -> String {
    let status = unsafe { libc::fcntl(received, libc::F_GETFL) };
    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    assert!(
        status < 0 || status & libc::O_NONBLOCK == 0,
        "{what}: the received eventfd took another sender's nonblocking open file"
    );
    format!(
        "{what}: F_GETFL={} errno={}",
        status.max(-1),
        if status < 0 { errno } else { 0 }
    )
}

/// A connection made through a relative path reaches the listener Detcore
/// knows by its absolute path without Detcore seeing it queued there. Linux
/// gives the first `accept` that connection (the oldest,
/// `unix_stream_connect` and `unix_accept`), so a blocking eventfd its
/// connecting socket sends arrives there; the nonblocking eventfd the
/// connection through the absolute path sends must not be taken for it.
/// Detcore leaves the listener's pairing unknown instead of pairing the
/// first accept with the absolute connect.
fn a_connection_through_a_relative_path_does_not_shift_the_pairing() {
    let directory = c"/tmp/hermit-detcore-scm-rights-relative";
    let path = c"/tmp/hermit-detcore-scm-rights-relative/listener";
    unsafe {
        libc::mkdir(directory.as_ptr(), 0o700);
        libc::unlink(path.as_ptr());
    }
    let address_of = |path: &[u8]| {
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (slot, byte) in address.sun_path.iter_mut().zip(path) {
            *slot = *byte as libc::c_char;
        }
        let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + path.len()) as u32;
        (address, length)
    };
    let absolute = address_of(path.to_bytes());
    let relative = address_of(b"listener");
    let at = |(address, _): &(libc::sockaddr_un, u32)| (&raw const *address).cast();
    let listener = unix_stream_socket();
    check(
        unsafe { libc::bind(listener, at(&absolute), absolute.1) },
        "bind",
    );
    check(unsafe { libc::listen(listener, 2) }, "listen");
    check(unsafe { libc::chdir(directory.as_ptr()) }, "chdir");
    let through_relative = unix_stream_socket();
    check(
        unsafe { libc::connect(through_relative, at(&relative), relative.1) },
        "connect through the relative path",
    );
    let through_absolute = unix_stream_socket();
    check(
        unsafe { libc::connect(through_absolute, at(&absolute), absolute.1) },
        "connect through the absolute path",
    );
    let accept = || {
        check(
            unsafe { libc::accept(listener, ptr::null_mut(), ptr::null_mut()) },
            "accept",
        )
    };
    let (first, second) = (accept(), accept());
    send_fds(through_absolute, &[eventfd(0, libc::EFD_NONBLOCK)]);
    send_fds(through_relative, &[eventfd(0, 0)]);
    let [received] = recv_fds(first, 0)[..] else {
        panic!("expected one descriptor");
    };
    println!(
        "{}",
        blocking_eventfd_status(received, "first accepted connection")
    );
    let [_] = recv_fds(second, 0)[..] else {
        panic!("expected one descriptor");
    };
    unsafe {
        libc::unlink(path.as_ptr());
        libc::chdir(c"/".as_ptr());
        libc::rmdir(directory.as_ptr());
    }
}

/// A child that changed its root with `chroot` connects through the same
/// absolute path that names the parent's first listener outside, and reaches
/// the listener at that path inside its root, the parent's second. Linux
/// gives the first listener's `accept` the parent's own later connection,
/// so the blocking eventfd that connection sends arrives there; the
/// nonblocking eventfd the child sends over its connection must not be taken
/// for it. Detcore leaves pairing unknown after a connect through an
/// absolute path from a changed root. The child calls only functions that
/// allocate nothing.
fn a_connection_from_a_chroot_does_not_shift_the_pairing() {
    let jail = c"/tmp/hermit-detcore-scm-rights-jail";
    let jail_tmp = c"/tmp/hermit-detcore-scm-rights-jail/tmp";
    let outside = c"/tmp/hermit-detcore-scm-rights-chroot-listener";
    let inside =
        c"/tmp/hermit-detcore-scm-rights-jail/tmp/hermit-detcore-scm-rights-chroot-listener";
    unsafe {
        libc::mkdir(jail.as_ptr(), 0o755);
        libc::mkdir(jail_tmp.as_ptr(), 0o755);
        libc::unlink(outside.as_ptr());
        libc::unlink(inside.as_ptr());
    }
    let address_of = |path: &[u8]| {
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (slot, byte) in address.sun_path.iter_mut().zip(path) {
            *slot = *byte as libc::c_char;
        }
        let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + path.len()) as u32;
        (address, length)
    };
    let (outer, inner) = (
        address_of(outside.to_bytes()),
        address_of(inside.to_bytes()),
    );
    let at = |(address, _): &(libc::sockaddr_un, u32)| (&raw const *address).cast();
    let listen_at = |address: &(libc::sockaddr_un, u32), path: &std::ffi::CStr| {
        let listener = unix_stream_socket();
        check(
            unsafe { libc::bind(listener, at(address), address.1) },
            "bind",
        );
        // The child's user namespace maps no user, so it needs the others bits.
        check(unsafe { libc::chmod(path.as_ptr(), 0o777) }, "chmod");
        check(unsafe { libc::listen(listener, 2) }, "listen");
        listener
    };
    let first_listener = listen_at(&outer, outside);
    let second_listener = listen_at(&inner, inside);
    let (sent, go) = (pipe(), pipe());
    // A fork into a user namespace of its own, where the child may chroot.
    let child = unsafe {
        libc::syscall(
            libc::SYS_clone,
            (libc::CLONE_NEWUSER | libc::SIGCHLD) as libc::c_ulong,
            0,
            0,
            0,
            0,
        )
    };
    assert!(child >= 0, "clone: {}", io::Error::last_os_error());
    if child == 0 {
        // Only the parent writes `go`, so the child sees its end of file if
        // the parent stops early.
        unsafe {
            libc::close(go.1);
            libc::close(sent.0);
        }
        let ok = unsafe { libc::chroot(jail.as_ptr()) } == 0
            && unsafe { libc::chdir(c"/".as_ptr()) } == 0;
        let client = unix_stream_socket();
        // The outside listener's path, which names the inside one here.
        if !ok || unsafe { libc::connect(client, at(&outer), outer.1) } != 0 {
            unsafe { libc::_exit(2) };
        }
        let nonblocking = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
        write_all(sent.1, b"c");
        // Send only once the parent's own connection is queued and accepted.
        let mut byte = [0u8];
        unsafe { libc::read(go.0, byte.as_mut_ptr().cast(), 1) };
        send_fds(client, &[nonblocking]);
        // Keep the eventfd until the parent has received.
        unsafe { libc::read(go.0, byte.as_mut_ptr().cast(), 1) };
        unsafe { libc::_exit(0) };
    }
    unsafe {
        libc::close(go.0);
        libc::close(sent.1);
    }
    assert_eq!(read_exact(sent.0, 1), b"c");
    let own = unix_stream_socket();
    check(
        unsafe { libc::connect(own, at(&outer), outer.1) },
        "connect",
    );
    let accepted = check(
        unsafe { libc::accept(first_listener, ptr::null_mut(), ptr::null_mut()) },
        "accept",
    );
    write_all(go.1, b"s");
    let child_side = check(
        unsafe { libc::accept(second_listener, ptr::null_mut(), ptr::null_mut()) },
        "accept",
    );
    let [_] = recv_fds(child_side, 0)[..] else {
        panic!("expected one descriptor from the child");
    };
    send_fds(own, &[eventfd(0, 0)]);
    let [received] = recv_fds(accepted, 0)[..] else {
        panic!("expected one descriptor");
    };
    println!(
        "{}",
        blocking_eventfd_status(received, "outside listener's connection")
    );
    write_all(go.1, b"e");
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(child as libc::pid_t, &mut status, 0) },
        child as libc::pid_t
    );
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "the child could not chroot and connect"
    );
    unsafe {
        libc::unlink(outside.as_ptr());
        libc::unlink(inside.as_ptr());
        libc::rmdir(jail_tmp.as_ptr());
        libc::rmdir(jail.as_ptr());
    }
}

/// A datagram the first socket of a `socketpair` sends to a socket it names
/// (`msg_name`) reaches that socket, not its peer (`unix_dgram_sendmsg`):
/// the nonblocking eventfd it carried must not be taken for the blocking one
/// the next datagram brings the peer.
fn an_addressed_datagram_does_not_take_the_peers_queue() {
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    let named = abstract_address(b"\0hermit-detcore-scm-rights-addressed");
    let other = check(
        unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) },
        "socket",
    );
    check(
        unsafe { libc::bind(other, (&raw const named.0).cast(), named.1) },
        "bind",
    );
    send_fds_to(left, &[eventfd(0, libc::EFD_NONBLOCK)], &named);
    send_fds(left, &[eventfd(0, 0)]);
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    println!("{}", blocking_eventfd_status(received, "peer"));
    let [_] = recv_fds(other, 0)[..] else {
        panic!("expected one descriptor");
    };
}

/// A `socketpair` datagram socket that connects to another socket sends to
/// it (`unix_dgram_connect`); connected back to its old peer, it reaches the
/// peer again. The nonblocking eventfd it sent while connected elsewhere
/// must not be taken for the blocking one it sends the peer afterwards.
fn a_reconnected_datagram_socket_does_not_take_the_peers_queue() {
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    let right_name = abstract_address(b"\0hermit-detcore-scm-rights-reconnected-peer");
    let other_name = abstract_address(b"\0hermit-detcore-scm-rights-reconnected-other");
    check(
        unsafe { libc::bind(right, (&raw const right_name.0).cast(), right_name.1) },
        "bind",
    );
    let other = check(
        unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) },
        "socket",
    );
    check(
        unsafe { libc::bind(other, (&raw const other_name.0).cast(), other_name.1) },
        "bind",
    );
    check(
        unsafe { libc::connect(left, (&raw const other_name.0).cast(), other_name.1) },
        "connect to another socket",
    );
    send_fds(left, &[eventfd(0, libc::EFD_NONBLOCK)]);
    check(
        unsafe { libc::connect(left, (&raw const right_name.0).cast(), right_name.1) },
        "connect back to the peer",
    );
    send_fds(left, &[eventfd(0, 0)]);
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    println!("{}", blocking_eventfd_status(received, "peer"));
    let [_] = recv_fds(other, 0)[..] else {
        panic!("expected one descriptor");
    };
}

/// An eventfd received after a receive with no room released an earlier
/// eventfd's message: every eventfd has one file identity, so only order
/// could tell the two apart, and the socket's queue no longer follows its
/// messages one for one. The later, blocking eventfd must never take the
/// released, nonblocking one's open file. Detcore refuses it instead, as main
/// refuses an eventfd it cannot classify, so `F_GETFL` through it fails with
/// EBADF where Linux reports a blocking descriptor.
fn eventfd_after_a_released_one_does_not_join_it() {
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    let released = eventfd(0, libc::EFD_NONBLOCK);
    send_fds(left, &[released]);
    let (none, ctrunc) = recv_fds_into(right, 0, 0);
    assert!(none.is_empty() && ctrunc, "Linux releases the descriptor");
    let kept = eventfd(5, 0);
    send_fds(left, &[kept]);
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let status = unsafe { libc::fcntl(received, libc::F_GETFL) };
    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    assert!(
        status < 0 || status & libc::O_NONBLOCK == 0,
        "the received eventfd took the released eventfd's nonblocking open file"
    );
    println!(
        "received eventfd: F_GETFL={} errno={}",
        status.max(-1),
        if status < 0 { errno } else { 0 }
    );
}

/// `recvmmsg` with `MSG_PEEK` peeks the first datagram for every entry, as
/// Linux does: each entry's descriptor names the first message's pipe end,
/// and both messages are still there to receive, in order.
fn recvmmsg_peek_names_the_first_message_for_every_entry() {
    let pipes: Vec<(c_int, c_int)> = (0..2).map(|_| pipe()).collect();
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    send_fds(left, &[pipes[0].1]);
    send_fds(left, &[pipes[1].1]);

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
    let peeked = unsafe {
        libc::recvmmsg(
            right,
            messages.as_mut_ptr(),
            2,
            libc::MSG_PEEK,
            ptr::null_mut(),
        )
    };
    assert!(peeked >= 1, "recvmmsg: {}", io::Error::last_os_error());
    for message in &messages[..peeked as usize] {
        let [fd] = received_fds(&message.msg_hdr)[..] else {
            panic!("expected one descriptor per peeked entry");
        };
        assert!(blocking(fd), "the first message's pipe end is blocking");
        write_all(fd, b"p");
        assert_eq!(read_exact(pipes[0].0, 1), b"p");
    }
    for (index, (read_end, _)) in pipes.iter().enumerate() {
        let [fd] = recv_fds(right, 0)[..] else {
            panic!("expected one descriptor");
        };
        assert!(blocking(fd), "pipe {index}'s write end is blocking");
        write_all(fd, b"r");
        assert_eq!(read_exact(*read_end, 1), b"r");
    }
    println!("{peeked} peeked entries named the first message");
}

/// A stream message long enough for Linux to queue in several buffers
/// carries its descriptors in the first only: a receive of one byte takes
/// them all, and the rest of the bytes, read without a control buffer, carry
/// none. The socket's queue still follows its messages, so the next
/// message's eventfd joins its sender's.
fn descriptors_of_a_long_stream_message_come_with_its_first_byte() {
    // Above half the default send buffer, so Linux splits it.
    const LENGTH: usize = 150_000;
    let (left, right) = socketpair(libc::SOCK_STREAM);
    let (read_end, write_end) = pipe();
    let mut payload = vec![b'x'; LENGTH];
    let mut sent = send_fds_with(left, &[write_end], &mut payload);
    while sent < LENGTH {
        let written = unsafe { libc::write(left, payload[sent..].as_ptr().cast(), LENGTH - sent) };
        assert!(written > 0, "write: {}", io::Error::last_os_error());
        sent += written as usize;
    }
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert!(blocking(received), "the sender's pipe end is blocking");
    write_all(received, b"w");
    assert_eq!(read_exact(read_end, 1), b"w");
    assert!(
        read_exact(right, LENGTH - 1)
            .iter()
            .all(|&byte| byte == b'x')
    );

    let counter = eventfd(0, 0);
    send_fds(left, &[counter]);
    let [received_counter] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    eventfd_write(received_counter, 3);
    assert_eq!(u64::from_ne_bytes(read_exact_once(counter)), 3);
    println!("the long message's descriptor came with its first byte; the next eventfd joined");
}

/// The open files a socket's messages carry are matched in order: a
/// `MSG_PEEK` receive and the receive after it both name the sent pipe end; a
/// receive with room for one of two descriptors names the first (Linux sets
/// `MSG_CTRUNC` and releases the second); and after a receive with no control
/// buffer discards a message, the next receive names its own message's pipe
/// end, not the discarded one.
fn queued_rights_follow_peek_truncation_and_discards() {
    let pipes: Vec<(c_int, c_int)> = (0..4).map(|_| pipe()).collect();
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    // Whether `received` is the write end of pipe `index`.
    let reaches = |received: c_int, index: usize| {
        assert!(blocking(received), "pipe {index}'s write end is blocking");
        write_all(received, b"w");
        assert_eq!(read_exact(pipes[index].0, 1), b"w");
    };

    send_fds(left, &[pipes[0].1]);
    let [peeked] = recv_fds(right, libc::MSG_PEEK)[..] else {
        panic!("expected one peeked descriptor");
    };
    let [taken] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert_ne!(peeked, taken);
    reaches(peeked, 0);
    reaches(taken, 0);

    send_fds(left, &[pipes[1].1, pipes[2].1]);
    // CMSG_LEN(sizeof(int)): room for one descriptor (CMSG_SPACE's padding
    // would hold a second).
    let room = unsafe { libc::CMSG_LEN(size_of::<c_int>() as u32) } as usize;
    let (truncated, ctrunc) = recv_fds_into(right, 0, room);
    assert!(ctrunc, "Linux sets MSG_CTRUNC");
    let [first] = truncated[..] else {
        panic!("expected one of two descriptors, got {truncated:?}");
    };
    reaches(first, 1);

    send_fds(left, &[pipes[2].1]);
    let (discarded, ctrunc) = recv_fds_into(right, 0, 0);
    assert!(
        discarded.is_empty() && ctrunc,
        "the message's descriptors are discarded"
    );
    send_fds(left, &[pipes[3].1]);
    let [last] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    reaches(last, 3);
    println!("peeked {peeked}, taken {taken}, truncated {first}, after a discard {last}");
}

#[test]
fn scm_rights_pipe_end_shares_the_open_file() {
    run_deterministically(received_pipe_end_shares_the_open_file);
}

#[test]
fn scm_rights_pipe_end_from_a_process_that_keeps_it() {
    run_deterministically(pipe_end_from_a_process_that_keeps_it_shares_the_open_file);
}

#[test]
fn scm_rights_over_a_connection_to_an_own_listener() {
    run_deterministically(descriptors_over_a_connection_to_an_own_listener_share_their_open_files);
}

#[test]
fn scm_rights_over_a_connection_from_a_child() {
    run_deterministically(descriptors_over_a_connection_from_a_child_share_their_open_files);
}

#[test]
fn scm_rights_connection_through_a_relative_path() {
    run_deterministically(a_connection_through_a_relative_path_does_not_shift_the_pairing);
}

#[test]
fn scm_rights_connection_from_a_chroot() {
    run_deterministically(a_connection_from_a_chroot_does_not_shift_the_pairing);
}

#[test]
fn scm_rights_addressed_datagram() {
    run_deterministically(an_addressed_datagram_does_not_take_the_peers_queue);
}

#[test]
fn scm_rights_reconnected_datagram_socket() {
    run_deterministically(a_reconnected_datagram_socket_does_not_take_the_peers_queue);
}

#[test]
fn scm_rights_eventfd_after_a_released_one() {
    run_deterministically(eventfd_after_a_released_one_does_not_join_it);
}

#[test]
fn scm_rights_recvmmsg_peek() {
    run_deterministically(recvmmsg_peek_names_the_first_message_for_every_entry);
}

#[test]
fn scm_rights_long_stream_message() {
    run_deterministically(descriptors_of_a_long_stream_message_come_with_its_first_byte);
}

#[test]
fn scm_rights_queue_follows_peek_truncation_and_discards() {
    run_deterministically(queued_rights_follow_peek_truncation_and_discards);
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
