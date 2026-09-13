/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Native controls for the real Detcore handlers. The RPC endpoint deliberately
//! holds admission; this is an ordered handler control, not a guest scheduler run.

use std::fs::File;
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Poll;

use futures::task::AtomicWaker;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Tool;
use reverie::syscalls::LocalMemory;

use super::*;
use crate::Config;
use crate::ThreadState;

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ProbeTool;

#[reverie::tool]
impl Tool for ProbeTool {
    type GlobalState = GlobalState;
    // (number of nested calls, 0=native, 1=partial then errno, 2=partial then Tool).
    type ThreadState = (usize, u8);

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        let state = guest.thread_state_mut();
        state.0 += 1;
        let (count, mode) = *state;
        if mode != 0 {
            let Syscall::Pwritev2(call) = syscall else {
                panic!("append must reach the nested tool as a positioned vector");
            };
            let iovec = guest.memory().read_value(call.iov().unwrap())?;
            let byte = guest
                .memory()
                .read_value(Addr::<u8>::from_raw(iovec.iov_base as usize).unwrap())?;
            assert_eq!(byte, b'A' + (count - 1) as u8);
            assert_eq!(iovec.iov_len, 3 - count);
            assert_eq!(call.flags(), libc::RWF_APPEND);
            if count == 1 {
                return Ok(1);
            }
            return if mode == 1 {
                Err(Errno::EIO.into())
            } else {
                Err(Error::Tool(anyhow::anyhow!("retained write-tool failure")))
            };
        }
        Ok(guest.inject(syscall).await?)
    }
}

type AppendTool = Detcore<ProbeTool>;

#[derive(Default)]
struct Admission {
    allow: AtomicBool,
    held: AtomicBool,
    wake: AtomicWaker,
    events: Mutex<Vec<&'static str>>,
}

impl Admission {
    fn admit(&self) {
        self.allow.store(true, Ordering::SeqCst);
        self.wake.wake();
    }
}

struct NativeStack {
    arena: Box<[u64; 128]>,
    used: usize,
    fail_commit: bool,
    live: Arc<AtomicBool>,
}

struct NativeStackGuard {
    _arena: Box<[u64; 128]>,
    live: Arc<AtomicBool>,
}

impl Drop for NativeStackGuard {
    fn drop(&mut self) {
        assert!(self.live.swap(false, Ordering::SeqCst));
    }
}

impl Stack for NativeStack {
    type StackGuard = NativeStackGuard;

    fn size(&self) -> usize {
        self.used
    }

    fn capacity(&self) -> usize {
        std::mem::size_of_val(self.arena.as_ref())
    }

    fn push<'stack, T>(&mut self, value: T) -> Addr<'stack, T> {
        assert!(std::mem::align_of::<T>() <= std::mem::align_of::<u64>());
        let start = self.used.next_multiple_of(std::mem::align_of::<T>());
        self.used = start + std::mem::size_of::<T>();
        assert!(self.used <= self.capacity());
        // The controls allocate only the same initialized iovec arrays used by
        // production. The boxed storage remains live in the returned guard.
        let pointer = unsafe { self.arena.as_mut_ptr().cast::<u8>().add(start).cast::<T>() };
        unsafe { pointer.write(value) };
        Addr::from_raw(pointer as usize).unwrap()
    }

    fn reserve<'stack, T>(&mut self) -> AddrMut<'stack, T> {
        // Old-source Fstat uses zeroed native scratch in the defect control.
        AddrMut::from_raw(self.push(unsafe { std::mem::zeroed::<T>() }).as_raw()).unwrap()
    }

    fn commit(self) -> Result<Self::StackGuard, Errno> {
        if self.fail_commit {
            return Err(Errno::EFAULT);
        }
        assert!(!self.live.swap(true, Ordering::SeqCst));
        Ok(NativeStackGuard {
            _arena: self.arena,
            live: self.live,
        })
    }
}

struct NativeGuest {
    config: Config,
    thread: ThreadState<(usize, u8)>,
    admission: Arc<Admission>,
    injected: Arc<Mutex<Vec<Syscall>>>,
    stack_live: Arc<AtomicBool>,
    fail_commit: bool,
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for NativeGuest {
    async fn send_rpc(
        &self,
        request: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        let response = match request.2 {
            GlobalRequest::RequestResources(_, _) => {
                self.admission.events.lock().unwrap().push("request");
                futures::future::poll_fn(|context| {
                    self.admission.wake.register(context.waker());
                    if self.admission.allow.load(Ordering::SeqCst) {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
                assert!(!self.admission.held.swap(true, Ordering::SeqCst));
                self.admission.events.lock().unwrap().push("admitted");
                GlobalResponse::RequestResources(ResumeStatus::Normal)
            }
            GlobalRequest::ReleaseAllResources => {
                assert!(self.admission.held.swap(false, Ordering::SeqCst));
                self.admission.events.lock().unwrap().push("release");
                GlobalResponse::ReleaseAllResources(())
            }
            request => panic!("unexpected write-control request: {request:?}"),
        };
        (None, response)
    }

    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<AppendTool> for NativeGuest {
    type Memory = LocalMemory;
    type Stack = NativeStack;

    fn tid(&self) -> reverie::Pid {
        reverie::Pid::from_raw(self.thread.dettid.as_raw())
    }
    fn pid(&self) -> reverie::Pid {
        self.tid()
    }
    fn ppid(&self) -> Option<reverie::Pid> {
        None
    }
    fn memory(&self) -> Self::Memory {
        LocalMemory::new()
    }
    fn thread_state(&self) -> &ThreadState<(usize, u8)> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<(usize, u8)> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("write handler must not read registers in this control")
    }
    async fn stack(&mut self) -> Self::Stack {
        assert!(!self.stack_live.load(Ordering::SeqCst));
        NativeStack {
            arena: Box::new([0; 128]),
            used: 0,
            fail_commit: self.fail_commit,
            live: self.stack_live.clone(),
        }
    }
    async fn daemonize(&mut self) {
        panic!("write handler must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
        let (number, arguments) = syscall.into_parts();
        let syscall = Syscall::from_raw(number, arguments);
        self.injected.lock().unwrap().push(syscall);
        if matches!(syscall, Syscall::Fstat(_)) {
            assert!(
                self.stack_live.load(Ordering::SeqCst),
                "fstat scratch must still be live"
            );
        }
        native_syscall(syscall)
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("write handler must not tail-inject")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("write handler must not set a timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("write handler must not set a timer")
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        panic!("write handler must not read a clock")
    }
}

fn native_syscall(call: Syscall) -> Result<i64, Errno> {
    assert!(matches!(
        call,
        Syscall::Write(_)
            | Syscall::Pwrite64(_)
            | Syscall::Writev(_)
            | Syscall::Pwritev(_)
            | Syscall::Pwritev2(_)
            | Syscall::Lseek(_)
            | Syscall::Fstat(_)
    ));
    let (number, args) = call.into_parts();
    // All descriptors belong to the individual test's files. Invalid pointers
    // are passed to Linux, never dereferenced by this native adapter.
    let result = unsafe {
        libc::syscall(
            number.id() as libc::c_long,
            args.arg0,
            args.arg1,
            args.arg2,
            args.arg3,
            args.arg4,
            args.arg5,
        )
    };
    if result == -1 {
        Err(Errno::last())
    } else {
        Ok(result)
    }
}

fn file() -> File {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"P").unwrap();
    assert_eq!(
        unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_SET) },
        0
    );
    file
}

fn guest(file: &File, admitted: bool) -> (AppendTool, NativeGuest) {
    let config = Config {
        sequentialize_threads: true,
        virtualize_metadata: false,
        deterministic_io: false,
        discover_live_file_metadata: false,
        ..Config::default()
    };
    let id = DetPid::from_raw(17);
    let tool = AppendTool::new(reverie::Pid::from_raw(17), &config);
    let mut thread = ThreadState::new(id, &config, (0, 0));
    thread.detpid = Some(id);
    // Supply this fixture's owned descriptor as an inherited regular stream.
    // The control calls the write handlers directly, not thread startup, and
    // does not assert that the host's stdio alias probe succeeded.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::fstat(file.as_raw_fd(), &mut stat) }, 0);
    thread
        .add_fd(
            file.as_raw_fd(),
            OFlag::from_bits_retain(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) } | libc::O_APPEND,
            ),
            FdType::Regular,
            Some(DetStat::from(&stat)),
        )
        .unwrap();
    thread
        .with_detfd(file.as_raw_fd(), |fd| {
            fd.set_resource(ResourceID::Device(Device::ContainerStdout))
        })
        .unwrap();
    let admission = Arc::new(Admission::default());
    if admitted {
        admission.admit();
    }
    (
        tool,
        NativeGuest {
            config,
            thread,
            admission,
            injected: Arc::new(Mutex::new(Vec::new())),
            stack_live: Arc::new(AtomicBool::new(false)),
            fail_commit: false,
        },
    )
}

async fn execute(tool: &AppendTool, guest: &mut NativeGuest, call: Syscall) -> Result<i64, Error> {
    match call {
        Syscall::Write(call) => tool.handle_write(guest, call).await,
        Syscall::Pwrite64(call) => tool.handle_pwrite64(guest, call).await,
        Syscall::Writev(call) => tool.handle_writev(guest, call).await,
        Syscall::Pwritev(call) => tool.handle_pwritev(guest, call).await,
        Syscall::Pwritev2(call) => tool.handle_pwritev2(guest, call).await,
        _ => unreachable!(),
    }
}

fn call(
    fd: i32,
    operation: &str,
    buffer: usize,
    length: usize,
    offset: i64,
    iov: usize,
) -> Syscall {
    match operation {
        "write" => syscalls::Write::new()
            .with_fd(fd)
            .with_buf(Addr::from_raw(buffer))
            .with_len(length)
            .into(),
        "pwrite" => syscalls::Pwrite64::new()
            .with_fd(fd)
            .with_buf(Addr::from_raw(buffer))
            .with_len(length)
            .with_offset(offset)
            .into(),
        "writev" => syscalls::Writev::new()
            .with_fd(fd)
            .with_iov(Addr::from_raw(iov))
            .with_len(2)
            .into(),
        "pwritev" => syscalls::Pwritev::new()
            .with_fd(fd)
            .with_iov(Addr::from_raw(iov))
            .with_iov_len(2)
            .with_pos_l(offset as u64)
            .with_pos_h(0)
            .into(),
        "pwritev2" => syscalls::Pwritev2::new()
            .with_fd(fd)
            .with_iov(Addr::from_raw(iov))
            .with_iov_len(2)
            .with_pos_l(offset as u64)
            .with_pos_h(0)
            .into(),
        _ => unreachable!(),
    }
}

fn vector(buffer: &[u8]) -> [libc::iovec; 2] {
    [
        libc::iovec {
            iov_base: buffer.as_ptr().cast_mut().cast(),
            iov_len: buffer.len(),
        },
        libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        },
    ]
}

fn snapshot(file: &File) -> (Vec<u8>, i64, i32) {
    use std::os::unix::fs::FileExt;
    let mut bytes = vec![0; file.metadata().unwrap().len() as usize];
    file.read_exact_at(&mut bytes, 0).unwrap();
    let offset = unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR) };
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    assert!(offset >= 0 && flags >= 0);
    (bytes, offset, flags)
}

#[tokio::test]
async fn inherited_append_waits_for_admission_before_positioning() {
    for ordinary in ["write", "writev"] {
        for positioned in ["pwrite", "pwritev"] {
            let file = file();
            let (tool, mut first) = guest(&file, false);
            let (_, mut sibling) = guest(&file, true);
            sibling.thread.file_metadata = first.thread.file_metadata.clone();
            let permit = first.admission.clone();
            let injected = first.injected.clone();
            let a = b"A";
            let a_iov = vector(a);
            let mut pending = Box::pin(execute(
                &tool,
                &mut first,
                call(
                    file.as_raw_fd(),
                    ordinary,
                    a.as_ptr() as usize,
                    1,
                    0,
                    a_iov.as_ptr() as usize,
                ),
            ));
            assert!(matches!(futures::poll!(pending.as_mut()), Poll::Pending));
            assert_eq!(*permit.events.lock().unwrap(), ["request"]);
            assert!(
                injected.lock().unwrap().is_empty(),
                "no kernel positioning or write may precede admission"
            );
            assert_eq!(snapshot(&file).0, b"P");
            assert_eq!(snapshot(&file).1, 0);
            let b = b"B";
            let b_iov = vector(b);
            assert_eq!(
                execute(
                    &tool,
                    &mut sibling,
                    call(
                        file.as_raw_fd(),
                        positioned,
                        b.as_ptr() as usize,
                        1,
                        0,
                        b_iov.as_ptr() as usize
                    )
                )
                .await
                .unwrap(),
                1
            );
            assert_eq!(snapshot(&file).0, b"PB");
            assert_eq!(
                snapshot(&file).1,
                0,
                "positioned sibling must not advance the shared offset"
            );
            permit.admit();
            assert_eq!(pending.await.unwrap(), 1);
            assert_eq!(snapshot(&file).0, b"PBA");
            assert_eq!(snapshot(&file).1, 3);
            assert_eq!(snapshot(&file).2 & libc::O_APPEND, 0);
            assert_eq!(
                *permit.events.lock().unwrap(),
                ["request", "admitted", "release"]
            );
        }
    }
}

#[tokio::test]
async fn inherited_append_reads_logical_flags_after_admission() {
    for initial in [false, true] {
        let file = file();
        let (tool, mut guest) = guest(&file, false);
        let fd = guest
            .thread
            .with_detfd(file.as_raw_fd(), |fd| fd.clone())
            .unwrap();
        fd.set_status_flags(libc::O_RDWR | if initial { libc::O_APPEND } else { 0 });
        let permit = guest.admission.clone();
        let mut pending = Box::pin(
            tool.handle_write(
                &mut guest,
                syscalls::Write::new()
                    .with_fd(file.as_raw_fd())
                    .with_buf(Addr::from_raw(b"A".as_ptr() as usize))
                    .with_len(1),
            ),
        );
        assert!(matches!(futures::poll!(pending.as_mut()), Poll::Pending));
        fd.set_status_flags(libc::O_RDWR | if initial { 0 } else { libc::O_APPEND });
        permit.admit();
        assert_eq!(pending.await.unwrap(), 1);
        let (bytes, offset, flags) = snapshot(&file);
        assert_eq!(
            bytes,
            if initial {
                b"A".as_slice()
            } else {
                b"PA".as_slice()
            }
        );
        assert_eq!(offset, if initial { 1 } else { 2 });
        assert_eq!(flags & libc::O_APPEND, 0);
    }
}

#[tokio::test]
async fn inherited_append_releases_after_scratch_or_descriptor_error() {
    for remove_fd in [false, true] {
        let file = file();
        let (tool, mut guest) = guest(&file, false);
        guest.fail_commit = !remove_fd;
        let metadata = guest.thread.file_metadata.clone();
        let permit = guest.admission.clone();
        let injected = guest.injected.clone();
        let live = guest.stack_live.clone();
        let mut pending = Box::pin(
            tool.handle_write(
                &mut guest,
                syscalls::Write::new()
                    .with_fd(file.as_raw_fd())
                    .with_buf(Addr::from_raw(b"A".as_ptr() as usize))
                    .with_len(1),
            ),
        );
        assert!(matches!(futures::poll!(pending.as_mut()), Poll::Pending));
        if remove_fd {
            metadata
                .lock()
                .unwrap()
                .file_handles
                .remove(&file.as_raw_fd());
        }
        permit.admit();
        assert_eq!(
            pending.await.unwrap_err().into_errno().unwrap(),
            if remove_fd {
                Errno::EBADF
            } else {
                Errno::EFAULT
            }
        );
        assert_eq!(
            *permit.events.lock().unwrap(),
            ["request", "admitted", "release"]
        );
        assert!(!live.load(Ordering::SeqCst));
        assert!(injected.lock().unwrap().is_empty());
        assert_eq!(snapshot(&file).0, b"P");
        assert_eq!(snapshot(&file).1, 0);
    }
}

#[tokio::test]
async fn inherited_append_preserves_partial_and_tool_error_results() {
    for operation in ["write", "pwrite"] {
        for mode in [1, 2] {
            let file = file();
            let (mut tool, mut guest) = guest(&file, true);
            tool.cfg.deterministic_io = true;
            guest.config.deterministic_io = true;
            guest.thread.record_or_replay.1 = mode;
            let result = execute(
                &tool,
                &mut guest,
                call(
                    file.as_raw_fd(),
                    operation,
                    b"AB".as_ptr() as usize,
                    2,
                    0,
                    0,
                ),
            )
            .await;
            if mode == 1 {
                assert_eq!(result.unwrap(), 1);
            } else {
                assert!(
                    matches!(result, Err(Error::Tool(error)) if error.to_string() == "retained write-tool failure")
                );
            }
            assert_eq!(guest.thread.record_or_replay.0, 2);
            assert_eq!(
                *guest.admission.events.lock().unwrap(),
                ["request", "admitted", "release"]
            );
            assert!(!guest.stack_live.load(Ordering::SeqCst));
        }
    }
}

#[test]
fn inherited_append_native_neighbors() {
    const CHILD: &str = "HERMIT_APPEND_NATIVE_NEIGHBOR_CHILD";
    if std::env::var_os(CHILD).is_none() {
        use std::os::unix::fs::FileExt;
        use std::process::Command;
        use std::process::Output;
        use std::process::Stdio;
        use std::time::Duration;
        use std::time::Instant;
        // Drain-independent native evidence: pipe capacity is not a test
        // prerequisite. The same combined 64 KiB cap is checked both while
        // the child runs and after its terminal status, including final writes.
        let stdout = tempfile::tempfile().unwrap();
        let stderr = tempfile::tempfile().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "syscalls::files::append_tests::inherited_append_native_neighbors",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env(
                "HERMIT_APPEND_NATIVE_CGROUP",
                std::fs::read_to_string("/proc/self/cgroup").unwrap(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.try_clone().unwrap()))
            .stderr(Stdio::from(stderr.try_clone().unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut failure = None;
        let mut kill_result = None;
        let status = loop {
            let status = child.try_wait().unwrap();
            let bytes = stdout.metadata().unwrap().len() + stderr.metadata().unwrap().len();
            if bytes > 64 * 1024 {
                failure = Some("native output exceeded 64 KiB");
            } else if status.is_none() && Instant::now() >= deadline {
                failure = Some("native neighbors exceeded five seconds");
            }
            if failure.is_some() {
                if status.is_none() {
                    kill_result = Some(child.kill());
                }
                break child.wait().unwrap();
            }
            if let Some(status) = status {
                break status;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let stdout_len = stdout.metadata().unwrap().len();
        let stderr_len = stderr.metadata().unwrap().len();
        if stdout_len + stderr_len > 64 * 1024 {
            failure.get_or_insert("native output exceeded 64 KiB during final drain");
        }
        let mut stdout_bytes = vec![0; stdout_len.min(64 * 1024) as usize];
        let mut stderr_bytes =
            vec![0; stderr_len.min(64 * 1024 - stdout_bytes.len() as u64) as usize];
        stdout.read_exact_at(&mut stdout_bytes, 0).unwrap();
        stderr.read_exact_at(&mut stderr_bytes, 0).unwrap();
        let output = Output {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        };
        print!("{}", String::from_utf8_lossy(&output.stdout));
        assert!(
            failure.is_none(),
            "native control {failure:?}, kill={kill_result:?}, actual stdout={stdout_len}, stderr={stderr_len}, result={output:?}"
        );
        assert!(
            output.status.success(),
            "native neighbors failed: {output:?}"
        );
        return;
    }
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    assert_eq!(
        cgroup,
        std::env::var("HERMIT_APPEND_NATIVE_CGROUP").unwrap()
    );
    println!(
        "append-native child pid={} cgroup={:?}",
        std::process::id(),
        cgroup
    );
    // Bound giant-count defect controls in their own process, without changing
    // any other test's process-wide limits or signal state.
    for (resource, limit) in [(libc::RLIMIT_FSIZE, 64 * 1024), (libc::RLIMIT_CPU, 2)] {
        let limit = libc::rlimit {
            rlim_cur: limit,
            rlim_max: limit,
        };
        assert_eq!(unsafe { libc::setrlimit(resource, &limit) }, 0);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(native_neighbors());
}

async fn native_neighbors() {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    assert!((4096..=16384).contains(&page));
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page * 3,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(mapping, libc::MAP_FAILED);
    assert_eq!(
        unsafe { libc::mprotect(mapping, page, libc::PROT_READ | libc::PROT_WRITE) },
        0
    );
    unsafe { std::ptr::write_bytes(mapping, b'A', page) };
    let good = mapping as usize;
    let mut cases = Vec::new();
    for operation in ["write", "pwrite", "writev", "pwritev", "pwritev2"] {
        for (buffer, length) in [
            (good, 0),
            (0, 0),
            (good, 16),
            (0, 16),
            (0x8000_0000_0000_0000, 0),
            (0x8000_0000_0000_0000, 16),
            (good + page - 8, 16),
            (usize::MAX - 15, 32),
            (good, isize::MAX as usize - 1),
            (good, isize::MAX as usize),
            (good, isize::MAX as usize + 1),
            (good, usize::MAX),
        ] {
            cases.push((operation, buffer, length, 0, false, 0));
        }
        for offset in [i64::MIN, -2, -1, 1, i64::MAX] {
            if operation.starts_with('p') {
                cases.push((operation, good, 16, offset, false, 0));
            }
        }
        cases.push((operation, good, 16, 0, true, 0));
    }
    cases.push(("pwritev2", good, 16, -1, false, 0));
    cases.push(("pwritev2", good, 16, 0, false, libc::RWF_NOAPPEND));
    cases.push(("pwritev2", good, 16, 0, false, libc::RWF_APPEND));
    cases.push(("pwritev2", good, 16, 0, false, i32::MIN));
    let expected = cases.len();
    let mut completed = 0;
    for (operation, buffer, length, offset, readonly, write_flags) in cases {
        let baseline = file();
        let candidate = file();
        // Reopen read-only through an owned /proc fd path to keep the reference
        // and candidate file content available to the test after EBADF.
        let baseline_ro = readonly
            .then(|| File::open(format!("/proc/self/fd/{}", baseline.as_raw_fd())).unwrap());
        let candidate_ro = readonly
            .then(|| File::open(format!("/proc/self/fd/{}", candidate.as_raw_fd())).unwrap());
        let baseline_target = baseline_ro.as_ref().unwrap_or(&baseline);
        let candidate_target = candidate_ro.as_ref().unwrap_or(&candidate);
        let baseline_flags = unsafe { libc::fcntl(baseline_target.as_raw_fd(), libc::F_GETFL) };
        assert!(baseline_flags >= 0);
        assert_eq!(
            unsafe {
                libc::fcntl(
                    baseline_target.as_raw_fd(),
                    libc::F_SETFL,
                    baseline_flags | libc::O_APPEND,
                )
            },
            0
        );
        let iov = [
            libc::iovec {
                iov_base: buffer as *mut libc::c_void,
                iov_len: length,
            },
            libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0,
            },
        ];
        let make = |fd| {
            let call = call(fd, operation, buffer, length, offset, iov.as_ptr() as usize);
            match call {
                Syscall::Pwritev2(call) => Syscall::Pwritev2(call.with_flags(write_flags)),
                call => call,
            }
        };
        let physical_before = unsafe { libc::fcntl(candidate_target.as_raw_fd(), libc::F_GETFL) };
        assert!(physical_before >= 0);
        let reference = native_syscall(make(baseline_target.as_raw_fd()));
        let (tool, mut guest) = guest(candidate_target, true);
        let actual = execute(&tool, &mut guest, make(candidate_target.as_raw_fd()))
            .await
            .map_err(|error| {
                error
                    .into_errno()
                    .expect("native adapter must preserve errno")
            });
        let physical_after = unsafe { libc::fcntl(candidate_target.as_raw_fd(), libc::F_GETFL) };
        assert!(physical_after >= 0);
        let left = snapshot(&baseline);
        let right = snapshot(&candidate);
        println!(
            "append-native {operation} len={length} offset={offset} readonly={readonly} flags={write_flags} reference={reference:?} actual={actual:?} sizes={}/{} positions={}/{}",
            left.0.len(),
            right.0.len(),
            left.1,
            right.1
        );
        assert_eq!(
            actual, reference,
            "{operation} len={length} offset={offset} flags={write_flags}"
        );
        assert_eq!(
            right.0, left.0,
            "native file content {operation} len={length}"
        );
        assert_eq!(
            right.1, left.1,
            "native OFD position {operation} len={length}"
        );
        assert_eq!(
            physical_after, physical_before,
            "candidate must preserve all physical flags"
        );
        assert_eq!(physical_after & libc::O_APPEND, 0);
        assert!(!guest.admission.held.load(Ordering::SeqCst));
        assert!(!guest.stack_live.load(Ordering::SeqCst));
        completed += 1;
    }
    assert_eq!(completed, expected);
    assert_eq!(completed, 84);
    assert_eq!(unsafe { libc::munmap(mapping, page * 3) }, 0);
}
