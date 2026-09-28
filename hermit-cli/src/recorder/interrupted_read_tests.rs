//! Real ptrace + Recorder/Replayer controls. This harness supplies no Detcore
//! scheduler, original Call, provider receipt or physical FD admission.
use std::marker::PhantomData;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;

use detcore::RecordOrReplay;
use reverie::ExitStatus;
use reverie::GlobalRPC;
use reverie::InjectedReadResult;
use reverie::InjectedSyscallEvent;
use reverie::InterruptedSyscall;
use reverie::Never;
use reverie::Signal;
use reverie::Subscription;
use reverie::TimerSchedule;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Read;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;

use super::*;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Config {
    replay: bool,
    data: PathBuf,
    fault: i32,
}
impl Config {
    fn delegate(&self) -> detcore::Config {
        detcore::Config {
            replay_data: Some(self.data.clone()),
            ..Default::default()
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Request {
    Handback,
    SignalHook,
    Consumed,
}
#[derive(Default)]
struct Log {
    events: Mutex<Vec<(Sysno, InjectedSyscallEvent)>>,
    order: Mutex<Vec<&'static str>>,
    terminal: Mutex<Vec<(i32, ExitStatus)>>,
    consumed: AtomicI32,
}
#[reverie::global_tool]
impl GlobalTool for Log {
    type Request = Request;
    type Response = ();
    type Config = Config;
    async fn receive_rpc(&self, _: Pid, request: Request) {
        match request {
            Request::Handback => self.order.lock().unwrap().push("handback"),
            Request::SignalHook => self.order.lock().unwrap().push("signal"),
            Request::Consumed => {
                self.consumed.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}
#[derive(Default, Serialize, Deserialize)]
struct Thread<S> {
    delegate: S,
    first: bool,
    prepared: usize,
    terminal: bool,
}
trait End: RecordOrReplay {
    fn check_end(state: &mut Self::ThreadState);
}
impl End for Recorder {
    fn check_end(_: &mut RecorderThreadState) {}
}
impl End for crate::replayer::Replayer {
    fn check_end(state: &mut crate::replayer::ReplayerThreadState) {
        assert_eq!(state.count, 4);
        assert!(
            matches!(state.next_event(), Err(bincode::error::DecodeError::Io { inner, .. })
            if inner.kind() == io::ErrorKind::UnexpectedEof)
        );
        assert!(
            matches!(state.next_debug_event(), Err(bincode::error::DecodeError::Io { inner, .. })
            if inner.kind() == io::ErrorKind::UnexpectedEof)
        );
    }
}
#[derive(Default)]
struct Harness<R: End> {
    delegate: R,
    cfg: detcore::Config,
    fault: i32,
}
#[reverie::tool]
impl<R: End + 'static> Tool for Harness<R> {
    type GlobalState = Log;
    type ThreadState = Thread<R::ThreadState>;
    fn new(pid: Pid, cfg: &Config) -> Self {
        let fault = cfg.fault;
        let cfg = cfg.delegate();
        Self {
            delegate: R::new(pid, &cfg),
            cfg,
            fault,
        }
    }
    fn init_thread_state(
        &self,
        tid: Tid,
        parent: Option<(Tid, &Self::ThreadState)>,
    ) -> Self::ThreadState {
        assert!(parent.is_none(), "single tracee control");
        Thread {
            delegate: self.delegate.init_thread_state(tid, None),
            first: true,
            prepared: 0,
            terminal: false,
        }
    }
    fn subscriptions(_: &Config) -> Subscription {
        let mut sub = Subscription::none();
        sub.syscalls([Sysno::read, Sysno::fcntl]);
        sub
    }
    fn observe_injected_syscalls(_: &Config) -> bool {
        true
    }
    fn observe_injected_syscall_preparation(_: &Config) -> bool {
        true
    }
    fn on_injected_syscall_observed(
        &self,
        tid: Pid,
        log: &Log,
        state: &mut Self::ThreadState,
        nr: Sysno,
        _: SyscallArgs,
        event: InjectedSyscallEvent,
    ) {
        log.events.lock().unwrap().push((nr, event));
        if nr == Sysno::read && event == InjectedSyscallEvent::Prepared {
            state.prepared += 1;
            if state.prepared == 2 {
                queue_signal(tid);
            }
        }
    }
    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        if let Syscall::Read(read) = call {
            ACTOR.store(guest.tid().as_raw(), Ordering::SeqCst);
            if std::mem::take(&mut guest.thread_state_mut().first) {
                let result = self
                    .delegate
                    .invoke_original_read(
                        &mut Adapter::<G, R> {
                            inner: guest,
                            cfg: &self.cfg,
                            marker: PhantomData,
                        },
                        read.with_len(0),
                    )
                    .await?;
                assert!(
                    matches!(result, InjectedReadResult::Complete(Ok(0))),
                    "initial native zero-count Read outcome: {result:?}"
                );
            }
            let result = self
                .delegate
                .invoke_original_read(
                    &mut Adapter::<G, R> {
                        inner: guest,
                        cfg: &self.cfg,
                        marker: PhantomData,
                    },
                    read,
                )
                .await?;
            match result {
                InjectedReadResult::Complete(result) => Ok(result?),
                InjectedReadResult::Interrupted(ticket)
                | InjectedReadResult::RecordedInterruption(ticket) => {
                    let mut bytes = [0; 2];
                    guest.memory().read_exact(read.buf().unwrap(), &mut bytes)?;
                    assert_eq!(bytes, [0x5a; 2]);
                    guest.send_rpc(Request::Handback).await;
                    guest
                        .finish_interrupted_syscall(ticket.clone(), None)
                        .await?;
                    Err(Error::Tool(anyhow::Error::new(ticket)))
                }
            }
        } else {
            assert!(matches!(call, Syscall::Fcntl(f) if matches!(f.cmd(), FcntlCmd::F_GETFL)));
            self.delegate
                .handle_syscall_event(
                    &mut Adapter::<G, R> {
                        inner: guest,
                        cfg: &self.cfg,
                        marker: PhantomData,
                    },
                    call,
                )
                .await
        }
    }
    async fn handle_signal_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        signal: Signal,
    ) -> Result<Option<Signal>, Errno> {
        assert_eq!(signal, Signal::SIGUSR1);
        guest.send_rpc(Request::SignalHook).await;
        Ok(Some(signal))
    }
    fn on_backend_thread_terminal(
        &self,
        tid: Pid,
        global: &Log,
        state: &mut Self::ThreadState,
        status: ExitStatus,
    ) {
        assert!(!state.terminal);
        state.terminal = true;
        TERMINALS.lock().unwrap().push((tid.as_raw(), status));
        global.terminal.lock().unwrap().push((tid.as_raw(), status));
    }
    async fn on_exit_thread<G: GlobalRPC<Log>>(
        &self,
        _: Pid,
        global: &G,
        mut state: Self::ThreadState,
        status: ExitStatus,
    ) -> Result<(), Error> {
        assert!(state.terminal);
        if self.fault == 0 {
            assert_eq!(status, ExitStatus::Exited(0));
            R::check_end(&mut state.delegate);
        } else {
            assert_eq!(status, ExitStatus::Signaled(Signal::SIGKILL, false));
        }
        CONSUMED.fetch_add(1, Ordering::SeqCst);
        global.send_rpc(Request::Consumed).await;
        Ok(())
    }
}
struct Adapter<'a, G, R> {
    inner: &'a mut G,
    cfg: &'a detcore::Config,
    marker: PhantomData<R>,
}
#[reverie::tool]
impl<G: Guest<Harness<R>>, R: End + 'static> GlobalRPC<detcore::GlobalState> for Adapter<'_, G, R> {
    async fn send_rpc(
        &self,
        _: <detcore::GlobalState as GlobalTool>::Request,
    ) -> <detcore::GlobalState as GlobalTool>::Response {
        panic!("delegate requested Detcore RPC")
    }
    fn config(&self) -> &detcore::Config {
        self.cfg
    }
}
#[reverie::tool]
impl<G: Guest<Harness<R>>, R: End + 'static> Guest<R> for Adapter<'_, G, R> {
    type Memory = G::Memory;
    type Stack = G::Stack;
    fn tid(&self) -> Tid {
        self.inner.tid()
    }
    fn pid(&self) -> Pid {
        self.inner.pid()
    }
    fn ppid(&self) -> Option<Pid> {
        self.inner.ppid()
    }
    fn memory(&self) -> Self::Memory {
        self.inner.memory()
    }
    fn thread_state(&self) -> &R::ThreadState {
        &self.inner.thread_state().delegate
    }
    fn thread_state_mut(&mut self) -> &mut R::ThreadState {
        &mut self.inner.thread_state_mut().delegate
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        self.inner.regs().await
    }
    async fn stack(&mut self) -> Self::Stack {
        self.inner.stack().await
    }
    async fn daemonize(&mut self) {
        panic!("no daemon")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        assert!(
            !self.inner.config().replay,
            "Replay injected a physical syscall"
        );
        self.inner.inject(call).await
    }
    async fn inject_original_read(&mut self, call: Read) -> InjectedReadResult {
        assert!(!self.inner.config().replay, "Replay injected Read");
        self.inner.inject_original_read(call).await
    }
    async fn await_recorded_read_interruption(
        &mut self,
        call: Read,
        signal: Signal,
    ) -> Result<InterruptedSyscall, Error> {
        assert!(self.inner.config().replay);
        assert_eq!(signal, Signal::SIGUSR1);
        let tid = self.inner.tid();
        let fault = self.inner.config().fault;
        // The real wait starts before its external producer sends the signal.
        // This is controller ordering, not a Detcore scheduler-grant proof.
        let mut waiting = Box::pin(self.inner.await_recorded_read_interruption(call, signal));
        assert!(futures_util::poll!(&mut waiting).is_pending());
        if fault == 0 {
            queue_signal(tid);
        } else {
            assert_eq!(
                unsafe { libc::syscall(libc::SYS_tgkill, tid.as_raw(), tid.as_raw(), fault) },
                0
            );
        }
        waiting.await
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> Never {
        panic!("no tail")
    }
    fn set_timer(&mut self, _: TimerSchedule) -> Result<(), Error> {
        panic!("no timer")
    }
    fn set_timer_precise(&mut self, _: TimerSchedule) -> Result<(), Error> {
        panic!("no timer")
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        panic!("no clock")
    }
}
fn queue_signal(tid: Pid) {
    let value = libc::sigval {
        sival_ptr: 0x5137usize as *mut libc::c_void,
    };
    assert_eq!(
        unsafe { libc::sigqueue(tid.as_raw(), libc::SIGUSR1, value) },
        0
    );
}
static TEST_LOCK: Mutex<()> = Mutex::new(());
static ACTOR: AtomicI32 = AtomicI32::new(-1);
static TERMINALS: Mutex<Vec<(i32, ExitStatus)>> = Mutex::new(Vec::new());
static CONSUMED: AtomicI32 = AtomicI32::new(0);
static SIGNALS: AtomicI32 = AtomicI32::new(0);
static SLOT: AtomicI32 = AtomicI32::new(-1);
extern "C" fn handler(signal: i32, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    assert_eq!(signal, libc::SIGUSR1);
    let info = unsafe { &*info };
    assert_eq!(info.si_code, libc::SI_QUEUE);
    assert_eq!(unsafe { info.si_value().sival_ptr } as usize, 0x5137);
    assert_eq!(unsafe { info.si_pid() }, unsafe { libc::getppid() });
    let flags = unsafe {
        libc::syscall(
            libc::SYS_fcntl,
            SLOT.load(Ordering::SeqCst),
            libc::F_GETFL,
            0,
            0,
            0,
            0,
        )
    };
    assert!(flags >= 0);
    assert_eq!(SIGNALS.fetch_add(1, Ordering::SeqCst), 0);
}
fn guest_body(fd: i32, address: usize, restart: bool) {
    SIGNALS.store(0, Ordering::SeqCst);
    SLOT.store(fd, Ordering::SeqCst);
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler as *const () as usize;
    action.sa_flags = libc::SA_SIGINFO | if restart { libc::SA_RESTART } else { 0 };
    assert_eq!(unsafe { libc::sigemptyset(&mut action.sa_mask) }, 0);
    assert_eq!(
        unsafe { libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()) },
        0
    );
    let buffer = unsafe { std::slice::from_raw_parts_mut(address as *mut u8, 4) };
    buffer.fill(0x5a);
    let first = unsafe { libc::syscall(libc::SYS_read, fd, address + 1, 2, 0, 0, 0) };
    if restart {
        assert_eq!(first, 2);
    } else {
        assert_eq!(first, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EINTR));
        assert_eq!(buffer, &[0x5a; 4]);
        assert_eq!(
            unsafe { libc::syscall(libc::SYS_read, fd, address + 1, 2, 0, 0, 0) },
            2
        );
    }
    assert_eq!(SIGNALS.load(Ordering::SeqCst), 1);
    assert_eq!(buffer, &[0x5a, b'X', b'Y', 0x5a]);
}
fn require_log(log: Log, replay: bool) {
    let events = log.events.lock().unwrap();
    let returned: Vec<_> = events
        .iter()
        .filter_map(|(nr, e)| {
            if *nr == Sysno::read {
                if let InjectedSyscallEvent::Returned(n) = e {
                    Some(*n)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();
    if replay {
        assert!(
            events.is_empty(),
            "Replay fabricated native observations: {events:?}"
        );
    } else {
        assert_eq!(returned, [0, 2]);
        assert_eq!(
            events
                .iter()
                .filter(|(nr, e)| *nr == Sysno::read
                    && *e == InjectedSyscallEvent::InterruptedBeforeEntry)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|(nr, e)| *nr == Sysno::read && *e == InjectedSyscallEvent::Entered)
                .count(),
            2
        );
    }
    assert_eq!(*log.order.lock().unwrap(), ["handback", "signal"]);
    let terminal = log.terminal.lock().unwrap();
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0].1, ExitStatus::Exited(0));
    assert_eq!(log.consumed.load(Ordering::SeqCst), 1);
    assert_eq!(unsafe { libc::kill(terminal[0].0, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    assert_eq!(
        unsafe { libc::waitpid(terminal[0].0, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}
fn roundtrip(restart: bool, fault: i32) {
    let _serial = TEST_LOCK.lock().unwrap();
    use std::io::Read as _;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let data = tempfile::tempdir().unwrap();
    // spawn_fn_with_config runs init_tracee, which closes3..256 before
    // entering the closure. Preserve exactly these two owned endpoints above
    // that bootstrap range; each low-number original is dropped before fork.
    // This delegate-only harness has no provider and proves no FD128 admission.
    fn preserved_endpoint(endpoint: UnixStream) -> UnixStream {
        use std::os::fd::FromRawFd;
        let raw = unsafe { libc::fcntl(endpoint.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 256) };
        assert!(
            raw >= 256,
            "preserve fixture endpoint: {}",
            io::Error::last_os_error()
        );
        let preserved = unsafe { UnixStream::from_raw_fd(raw) };
        drop(endpoint);
        preserved
    }
    let (slot, peer) = UnixStream::pair().unwrap();
    let (mut slot, mut peer) = (preserved_endpoint(slot), preserved_endpoint(peer));
    slot.set_nonblocking(true).unwrap();
    peer.write_all(b"XY").unwrap();
    let address = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(address, libc::MAP_FAILED);
    let fd = slot.as_raw_fd();
    let raw = address as usize;
    let (out, log) = reverie_ptrace::testing::test_fn_with_config::<Harness<Recorder>, _>(
        move || guest_body(fd, raw, restart),
        Config {
            replay: false,
            data: data.path().to_path_buf(),
            fault: 0,
        },
        true,
    )
    .expect("record backend");
    assert_eq!(out.status, ExitStatus::Exited(0));
    require_log(log, false);
    peer.write_all(b"ZZ").unwrap();
    TERMINALS.lock().unwrap().clear();
    CONSUMED.store(0, Ordering::SeqCst);
    let replay =
        reverie_ptrace::testing::test_fn_with_config::<Harness<crate::replayer::Replayer>, _>(
            move || guest_body(fd, raw, restart),
            Config {
                replay: true,
                data: data.path().to_path_buf(),
                fault,
            },
            true,
        );
    if fault == 0 {
        let (out, log) = replay.expect("replay backend");
        assert_eq!(out.status, ExitStatus::Exited(0));
        require_log(log, true);
    } else {
        if fault == libc::SIGKILL {
            let (out, log) = replay.expect("positive cancellation of absent signal wait");
            assert_eq!(out.status, ExitStatus::Signaled(Signal::SIGKILL, false));
            assert!(log.events.lock().unwrap().is_empty());
            assert!(log.order.lock().unwrap().is_empty());
        } else {
            let error = replay
                .err()
                .expect("different first signal must refuse before handler or result");
            assert!(
                error.to_string().contains("EPROTO"),
                "original mismatch diagnostic lost: {error}"
            );
        }
        let actor = ACTOR.load(Ordering::SeqCst);
        assert_eq!(
            *TERMINALS.lock().unwrap(),
            [(actor, ExitStatus::Signaled(Signal::SIGKILL, false))]
        );
        assert_eq!(CONSUMED.load(Ordering::SeqCst), 1);
        assert_eq!(unsafe { libc::kill(actor, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        assert_eq!(
            unsafe { libc::waitpid(actor, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
    let mut untouched = [0; 2];
    slot.read_exact(&mut untouched).unwrap();
    assert_eq!(&untouched, b"ZZ", "Replay consumed physical bytes");
    assert_eq!(unsafe { libc::munmap(address, 4096) }, 0);
}
#[test]
fn interrupted_read_real_record_replay_eintr_handler_then_retry() {
    roundtrip(false, 0);
}
#[test]
fn interrupted_read_real_record_replay_restart_handler_then_retry() {
    roundtrip(true, 0);
}

#[test]
fn interrupted_read_replay_absent_signal_retains_owned_cancellation() {
    roundtrip(true, libc::SIGKILL);
}
#[test]
fn interrupted_read_replay_different_first_signal_is_explicit_divergence() {
    // These are refusal/cleanup controls, not faithful signal-delivery proof.
    // The implementation must never suppress them to hunt a later SIGUSR1.
    for signal in [libc::SIGUSR2, libc::SIGSTOP, libc::SIGSTKFLT] {
        roundtrip(true, signal);
    }
}
