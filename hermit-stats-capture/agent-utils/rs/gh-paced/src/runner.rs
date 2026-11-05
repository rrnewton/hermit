//! Running the real `gh`: transparent pass-through with stderr teed through the pushback scanner.
//!
//! The child inherits stdin (unless gh-paced had to buffer it for the content guard) and stdout
//! unchanged. Stderr goes through gh-paced so it can be scanned while it streams: through a
//! pseudo-terminal when gh-paced's own stderr is a terminal (so gh still sees a terminal and
//! keeps its colours, spinners and prompts), otherwise through a pipe. User-sent INT, TERM, HUP
//! and QUIT are forwarded to the child; terminal-generated signals already reach it because it
//! shares the foreground process group. A child killed by a signal makes gh-paced die by the
//! same signal after bookkeeping, so callers see the same status as from gh itself.
//!
//! For `gh api --include`, which prints the HTTP status line and response headers on stdout,
//! stdout is teed the same way (through a pseudo-terminal when gh-paced's stdout is a terminal)
//! and fed to the scanner's header parser, so a `Retry-After` or `x-ratelimit-remaining: 0`
//! header is seen. A command with a deadline (a watch) is sent SIGTERM when the deadline passes
//! and SIGKILL [`KILL_GRACE_SECS`] later. The write lease descriptor and the snapshot lock
//! descriptor are left open across exec, so the lease and the snapshot live as long as gh does.
//!
//! Each teed stream has two threads. The reader takes bytes from gh, feeds them to the scanner
//! first, then queues them; the writer copies the queue to gh-paced's own stream. A slow
//! consumer therefore never delays scanning, and gh feels the same back-pressure it would
//! without gh-paced once [`TEE_QUEUE_BYTES`] are queued. After gh exits the reader stops
//! applying back-pressure (all gh wrote is then at most a pipe's worth, already in the kernel)
//! and stops at end of file, or once the stream has been silent for [`TEE_IDLE_SECS`] (a
//! descendant holds it open), or [`TEE_AFTER_EXIT_SECS`] after gh exited, or after
//! [`TEE_AFTER_EXIT_BYTES`] more bytes (a descendant keeps writing). Silence is measured from
//! the reader's last read, so a reader held up in the pushback hook (waiting for the state
//! file's lock) can reach a cutoff with gh's own output still unread; before it stops at one it
//! therefore reads, scans and queues what can be read at once, up to [`READY_SCAN_BYTES`] (at
//! least what a pipe holds by default, so all gh wrote unless the pipe was resized). The reader
//! reads its stream non-blocking ([`ReadEnd`]), so no read waits for output: a poll that
//! reported the stream readable can be stale by the time of the read (on a pseudo-terminal, a
//! descendant can close the last slave descriptor, which makes the master report a hang-up, and
//! open the terminal again before the read), and a blocking read would then wait for the
//! descendant's next write, past every cutoff. The writer
//! is then waited for without a time limit, so every byte read reaches the consumer however
//! slowly it reads.
//! A stream that a descendant still holds when the reader stops is not closed: once the writer
//! has delivered everything read so far, the still-open stream is handed to a separate drainer
//! process ([`Invocation::drain`], `gh-paced --drain`), which copies the rest to the same
//! consumer until the last writer closes it or the consumer goes away, scanning stderr for
//! pushback as it goes. gh-paced itself then finishes and exits without waiting for the
//! descendant, as gh would have, and nothing the descendant writes is lost.
//! If the consumer goes away (a write fails, as with EPIPE after `| head -n1` exits), the rest
//! is discarded and the reader stops and closes its end, so gh's next write to that stream
//! fails (EPIPE, and SIGPIPE by default; EIO through a pseudo-terminal) just as it would have
//! writing to the consumer directly, instead of gh writing on, and perhaps paginating on, into
//! gh-paced. What was scanned before that point still counts. An INT,
//! TERM, HUP or QUIT that arrives after gh has exited, sent by a process or typed at the
//! terminal, abandons whatever is still undelivered and is reported in [`Ran::late_signal`], so gh-paced can die by it after its bookkeeping
//! rather than wait indefinitely on a consumer that has stopped reading. Each reader then
//! scans what it can read from gh's stream at once, up to [`READY_SCAN_BYTES`], without
//! delivering it, and stops. gh has exited, so its pipe holds at most one pipe's capacity,
//! which the byte bound covers unless the pipe was resized (64 KiB by default on x86-64;
//! 1 MiB is also the default limit on resizing by an unprivileged process). gh-paced waits for the readers up to [`LATE_SCAN_WAIT_SECS`] longer than
//! [`Invocation::hook_wait_secs`], since a reader may first have to wait in the hook, then
//! feeds the end of stdout to the scanner whether or not its reader got there, so a header
//! block already read counts as one the output cut short. The writers write to
//! descriptors 1 and 2 directly, without the standard library's stream locks, so a writer stuck
//! on a consumer that stopped reading cannot hold up gh-paced's own messages (see
//! [`write_stderr_bounded`]).
//!
//! The reader calls [`Invocation::on_pushback`] whenever the scanner's pushback signals change,
//! before queueing the chunk that changed them, so the cooldown is recorded while gh's output may
//! still be waiting for the consumer, not only after it has all been delivered.

use crate::pushback::Scanner;
use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How the child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Normal exit with this status.
    Code(i32),
    /// Killed by this signal.
    Signal(i32),
}

/// Seconds between SIGTERM and SIGKILL when a deadline passes.
pub const KILL_GRACE_SECS: f64 = 5.0;

/// Bytes a tee holds for a slow consumer before gh has to wait for it, while gh runs.
pub const TEE_QUEUE_BYTES: usize = 8 << 20;

/// After gh exits, a tee stops reading once its stream has been silent this long.
pub const TEE_IDLE_SECS: f64 = 2.0;

/// After gh exits, a tee stops reading this long after the exit, whatever is still arriving.
pub const TEE_AFTER_EXIT_SECS: f64 = 5.0;

/// After gh exits, a tee stops reading after this many more bytes.
pub const TEE_AFTER_EXIT_BYTES: usize = 64 << 20;

/// Before a tee's reader stops at a cutoff after gh's exit, it reads, scans and queues up to this
/// many bytes that it can read from gh's stream at once; after a late signal it scans up to this
/// many, delivering none of them.
pub const READY_SCAN_BYTES: usize = 1 << 20;

/// After a late signal, how long gh-paced waits for each tee's reader to finish scanning before
/// taking the scanner, beyond [`Invocation::hook_wait_secs`].
pub const LATE_SCAN_WAIT_SECS: f64 = 1.0;

/// Called from a tee's reader thread with a copy of the scanner whenever its pushback signals
/// change, so a cooldown can be recorded before gh's output has been delivered.
#[derive(Clone)]
pub struct PushbackHook(pub Arc<dyn Fn(&Scanner) + Send + Sync>);

impl fmt::Debug for PushbackHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PushbackHook")
    }
}

/// The command that continues delivering a stream gh-paced stops reading while a process gh
/// started still holds it (see the module documentation). The stream becomes the command's
/// standard input; `err` or `out` is appended to [`DrainCommand::args`] to say which of
/// gh-paced's streams it copies to.
#[derive(Debug, Clone)]
pub struct DrainCommand {
    /// Program to run (gh-paced's own executable).
    pub program: std::path::PathBuf,
    /// Arguments before the stream name.
    pub args: Vec<String>,
}

/// One child process to run.
#[derive(Debug, Clone)]
pub struct Invocation<'a> {
    /// Absolute path of the real gh.
    pub program: &'a Path,
    /// Arguments after the program name.
    pub args: &'a [String],
    /// `Some(bytes)`: feed exactly these bytes on stdin (already read for the content guard).
    /// `None`: the child inherits stdin.
    pub stdin: Option<Vec<u8>>,
    /// Extra environment variables for the child.
    pub env: Vec<(String, String)>,
    /// Stop the child once it has run this many seconds (SIGTERM, then SIGKILL after
    /// [`KILL_GRACE_SECS`]). `None`: no limit.
    pub deadline_secs: Option<f64>,
    /// Descriptors the child keeps open across exec (the write lease, the snapshot lock).
    pub keep_fds: Vec<RawFd>,
    /// Tee stdout through gh-paced and feed it to [`Scanner::feed_headers`].
    pub scan_stdout: bool,
    /// Called as soon as the scanner's pushback signals change (see [`PushbackHook`]).
    pub on_pushback: Option<PushbackHook>,
    /// How long one call of `on_pushback` can block, seconds (it waits for the state file's
    /// lock). After a late signal gh-paced waits up to this plus [`LATE_SCAN_WAIT_SECS`] for the
    /// readers to scan what gh wrote, so a reader caught waiting in the hook still gets there.
    pub hook_wait_secs: f64,
    /// Hands a stream still held by a descendant after gh exits to a drainer process. `None`:
    /// such a stream is closed when the reader stops, and later output to it is lost
    /// ([`Ran::cut_off`]).
    pub drain: Option<DrainCommand>,
}

/// How an interactive run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ran {
    /// The child's exit.
    pub exit: Exit,
    /// The child was stopped because its deadline passed.
    pub deadline_hit: bool,
    /// A user-sent signal that arrived after gh exited, while its output was still being
    /// delivered; the undelivered rest was abandoned.
    pub late_signal: Option<i32>,
    /// A process gh started still held one of the teed streams when gh-paced stopped reading
    /// it, and no drainer could take the stream over, so anything that process wrote to it
    /// afterwards was lost.
    pub cut_off: bool,
}

/// Most bytes of each stream a captured run keeps ([`Runner::capture`]): 1 MiB.
pub const MAX_CAPTURE_BYTES: usize = 1 << 20;

/// Output of a captured (non-interactive) run.
#[derive(Debug, Clone)]
pub struct Captured {
    /// How the child ended (`Signal(9)` after a timeout kill).
    pub exit: Exit,
    /// What it wrote to stdout, up to [`MAX_CAPTURE_BYTES`].
    pub stdout: Vec<u8>,
    /// What it wrote to stderr, up to [`MAX_CAPTURE_BYTES`].
    pub stderr: Vec<u8>,
    /// The child was killed because it ran past the timeout.
    pub timed_out: bool,
    /// It wrote more than [`MAX_CAPTURE_BYTES`] to stdout; `stdout` holds only the first part.
    pub stdout_cut: bool,
    /// It wrote more than [`MAX_CAPTURE_BYTES`] to stderr; `stderr` holds only the first part.
    pub stderr_cut: bool,
}

/// Why [`Runner::run`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunError {
    /// What went wrong, for the caller to print.
    pub message: String,
    /// Whether gh had been started by then. False only when gh's program never ran (it could
    /// not be executed), so it cannot have written anything; true for a failure after that,
    /// such as one while waiting for gh to exit.
    pub started: bool,
}

/// Something that can run gh. Tests substitute a fake.
pub trait Runner: Send + Sync {
    /// Run interactively: stdout inherited (or teed, see [`Invocation::scan_stdout`]), stderr
    /// streamed to our stderr and fed to `scanner`.
    fn run(&self, inv: Invocation<'_>, scanner: &mut Scanner) -> Result<Ran, RunError>;
    /// Run with stdout and stderr captured, killing the child after `timeout_secs`. Each stream
    /// keeps its first [`MAX_CAPTURE_BYTES`], and reports whether more followed.
    fn capture(&self, inv: Invocation<'_>, timeout_secs: f64) -> Result<Captured, String>;
}

/// Spawns the real gh.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealRunner;

static CHILD_PID: AtomicI32 = AtomicI32::new(0);

/// A forwarded signal that arrived when there was no child to forward it to.
static LATE_SIGNAL: AtomicI32 = AtomicI32::new(0);

const FORWARDED: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

extern "C" fn forward_signal(
    sig: libc::c_int,
    info: *mut libc::siginfo_t,
    _ctx: *mut libc::c_void,
) {
    // Only signals sent by a process (kill, sigqueue, tgkill: si_code <= 0) are forwarded.
    // Terminal-generated signals (si_code SI_KERNEL) already went to the whole foreground
    // process group, child included. With no child running, every signal, whoever sent it, is
    // for gh-paced itself: a terminal's INT after gh has exited must end gh-paced just as it
    // would have ended gh.
    // SAFETY: the kernel passes a valid siginfo_t to an SA_SIGINFO handler.
    let code = if info.is_null() {
        0
    } else {
        unsafe { (*info).si_code }
    };
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid <= 0 {
        LATE_SIGNAL.store(sig, Ordering::SeqCst);
    } else if code <= 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

fn signal_set() -> libc::sigset_t {
    // SAFETY: sigemptyset/sigaddset initialise and fill a local sigset_t.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in FORWARDED {
            libc::sigaddset(&mut set, s);
        }
        set
    }
}

/// Install the forwarding handler for each signal in [`FORWARDED`] that is not ignored, and
/// report which ones got it. An ignored signal (for example SIGHUP under `nohup`) stays ignored,
/// so the child inherits the same disposition it would have had without gh-paced.
fn install_forwarders() -> [bool; FORWARDED.len()] {
    let mut installed = [false; FORWARDED.len()];
    for (i, s) in FORWARDED.into_iter().enumerate() {
        // SAFETY: querying the current disposition, then installing a handler with a valid
        // function pointer and an empty mask.
        unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(s, std::ptr::null(), &mut current) == 0
                && current.sa_sigaction == libc::SIG_IGN
            {
                continue;
            }
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = forward_signal as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            installed[i] = libc::sigaction(s, &action, std::ptr::null_mut()) == 0;
        }
    }
    installed
}

/// Die by `sig` (after resetting its handler), so the caller sees the child's status. Core dumps
/// are suppressed. Falls back to `exit(128 + sig)`.
pub fn die_by_signal(sig: i32) -> ! {
    let core = [
        libc::SIGQUIT,
        libc::SIGABRT,
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGFPE,
        libc::SIGILL,
        libc::SIGSYS,
        libc::SIGTRAP,
        libc::SIGXCPU,
        libc::SIGXFSZ,
    ];
    // SAFETY: plain libc calls on this process.
    unsafe {
        if core.contains(&sig) {
            let zero = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &zero);
        }
        libc::signal(sig, libc::SIG_DFL);
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        libc::raise(sig);
    }
    std::process::exit(128 + sig)
}

/// A pseudo-terminal pair: the master (read by gh-paced) and the slave (the child's stderr).
struct Pty {
    master: File,
    slave: File,
}

/// Open a pseudo-terminal whose window size is copied from descriptor `size_from`.
fn open_pty(size_from: RawFd) -> Option<Pty> {
    // SAFETY: standard pty allocation; every returned descriptor is checked and owned.
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if master < 0 {
            return None;
        }
        let master_file = File::from_raw_fd(master);
        if libc::grantpt(master) != 0 || libc::unlockpt(master) != 0 {
            return None;
        }
        let mut name = [0 as libc::c_char; 128];
        if libc::ptsname_r(master, name.as_mut_ptr(), name.len()) != 0 {
            return None;
        }
        let slave = libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        );
        if slave < 0 {
            return None;
        }
        let slave_file = File::from_raw_fd(slave);
        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave, &mut tio) == 0 {
            libc::cfmakeraw(&mut tio);
            libc::tcsetattr(slave, libc::TCSANOW, &tio);
        }
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(size_from, libc::TIOCGWINSZ, &mut ws) == 0 {
            libc::ioctl(master, libc::TIOCSWINSZ, &ws);
        }
        Some(Pty {
            master: master_file,
            slave: slave_file,
        })
    }
}

fn is_tty(fd: RawFd) -> bool {
    // SAFETY: isatty on a standard descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

fn decode(status: std::process::ExitStatus) -> Exit {
    match (status.code(), status.signal()) {
        (Some(c), _) => Exit::Code(c),
        (None, Some(s)) => Exit::Signal(s),
        (None, None) => Exit::Code(1),
    }
}

/// Which of our streams a tee copies to, and how the scanner reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    /// stderr: scanned for gh's error messages.
    Err,
    /// stdout of `gh api --include`: scanned for the status line and headers.
    Out,
}

impl Stream {
    /// The name the drainer takes on its command line.
    pub fn name(self) -> &'static str {
        match self {
            Stream::Err => "err",
            Stream::Out => "out",
        }
    }

    /// The stream named `name` on the drainer's command line.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "err" => Some(Stream::Err),
            "out" => Some(Stream::Out),
            _ => None,
        }
    }

    fn fd(self) -> RawFd {
        match self {
            Stream::Err => 2,
            Stream::Out => 1,
        }
    }
}

/// The queue between a tee's reader and writer.
#[derive(Default)]
struct TeeQueue {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    /// The reader has stopped; the writer exits once the queue is empty.
    closed: bool,
    /// Our stream refused a write; later bytes are discarded.
    broken: bool,
    /// Delivery was abandoned by a late signal (see [`Tee::abandon_late`]).
    late: bool,
}

/// State shared by one tee's reader, its writer, and the thread waiting for gh.
#[derive(Default)]
struct Tee {
    queue: Mutex<TeeQueue>,
    changed: Condvar,
    /// gh has exited.
    child_exited: AtomicBool,
    /// The reader has stopped and fed the end of its stream to the scanner, so everything it
    /// read has been scanned.
    scanned: AtomicBool,
}

impl Tee {
    fn lock(&self) -> std::sync::MutexGuard<'_, TeeQueue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queue `bytes`, waiting while the queue is full and gh is still running.
    fn push(&self, bytes: Vec<u8>) {
        let mut q = self.lock();
        while q.bytes >= TEE_QUEUE_BYTES && !q.broken && !self.child_exited.load(Ordering::SeqCst) {
            q = self
                .changed
                .wait_timeout(q, Duration::from_millis(100))
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
        if !q.broken {
            q.bytes += bytes.len();
            q.chunks.push_back(bytes);
        }
        drop(q);
        self.changed.notify_all();
    }

    fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_all();
    }

    fn child_exited(&self) {
        self.child_exited.store(true, Ordering::SeqCst);
        self.changed.notify_all();
    }

    /// Stop delivering after a late signal: as [`Tee::abandon`], but the reader first scans what
    /// it can read from gh's stream at once (gh has exited, so that is output gh already wrote).
    fn abandon_late(&self) {
        self.lock().late = true;
        self.abandon();
    }

    /// Stop delivering: discard what is queued and anything read later.
    fn abandon(&self) {
        let mut q = self.lock();
        q.broken = true;
        q.chunks.clear();
        q.bytes = 0;
        drop(q);
        self.changed.notify_all();
    }
}

/// Wait up to `ms` for `fd` to become readable (or hung up). True when a read will not block.
fn readable(fd: RawFd, ms: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: polling one valid descriptor with a stack pollfd.
    let n = unsafe { libc::poll(&mut p, 1, ms) };
    n > 0
}

/// Write all of `bytes` to `fd` with plain `write` calls, retrying on EINTR. False on any
/// other error (EPIPE when the consumer has gone). Takes no lock, so it never waits for another
/// thread's write; it can still block while the consumer is not reading.
pub fn write_fd_all(fd: RawFd, mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        // SAFETY: writing from a valid slice to a descriptor; the length is the slice's.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        bytes = &bytes[usize::try_from(n).unwrap_or(0)..];
    }
    true
}

/// Write `bytes` to stderr, giving up at `deadline`. The write runs on its own thread, which is
/// left behind when the deadline passes (the process is about to exit), so a consumer that has
/// stopped reading cannot hold the caller past the deadline. True when the bytes were written.
pub fn write_stderr_bounded(bytes: Vec<u8>, deadline: Instant) -> bool {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return false;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(write_fd_all(2, &bytes));
    });
    matches!(rx.recv_timeout(left), Ok(true))
}

/// Feed `chunk` (or, with `None`, the end of the stream) to the scanner, then call the hook with
/// a copy of the scanner if its signals differ from `published`.
fn scan_and_publish(
    scanner: &Mutex<Scanner>,
    which: Stream,
    chunk: Option<&[u8]>,
    hook: Option<&PushbackHook>,
    published: &mut String,
) {
    let changed = scan(scanner, which, chunk, hook.is_some(), published);
    publish(hook, changed);
}

/// Feed `chunk` (or, with `None`, the end of the stream) to the scanner. Returns a copy of the
/// scanner for the hook when there is one and the scanner's signals differ from `published`.
fn scan(
    scanner: &Mutex<Scanner>,
    which: Stream,
    chunk: Option<&[u8]>,
    hooked: bool,
    published: &mut String,
) -> Option<Scanner> {
    match scanner.lock() {
        Ok(mut s) => {
            match (which, chunk) {
                (Stream::Err, Some(c)) => s.feed(c),
                (Stream::Out, Some(c)) => s.feed_headers(c),
                (Stream::Err, None) => {}
                (Stream::Out, None) => s.end_of_stdout(),
            }
            let key = s.signal_key();
            if hooked && !s.matched().is_empty() && key != *published {
                *published = key;
                Some(s.clone())
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

/// Call the hook with `snapshot`, a copy taken by [`scan`]. Called with the scanner unlocked:
/// recording the cooldown takes the state file's lock.
fn publish(hook: Option<&PushbackHook>, snapshot: Option<Scanner>) {
    if let (Some(h), Some(snapshot)) = (hook, snapshot) {
        (h.0)(&snapshot);
    }
}

/// Read `src` until it ends (EIO from a pty master counts as the end), feeding the scanner
/// (and calling the pushback hook) before queueing each chunk, and stopping early after gh's
/// exit, or as soon as delivery is abandoned, as described in the module documentation.
/// Returns `src` when it stopped at one of the after-exit cutoffs, so the stream, which a
/// descendant may still write to, can be handed to a drainer; otherwise it drops `src`, which
/// closes gh-paced's end of gh's stream.
fn tee_reader(
    src: File,
    which: Stream,
    scanner: Arc<Mutex<Scanner>>,
    tee: Arc<Tee>,
    hook: Option<PushbackHook>,
) -> Option<File> {
    let fd = src.as_raw_fd();
    let mut src = ReadEnd::new(src);
    let mut buf = vec![0u8; 64 * 1024];
    let mut published = String::new();
    let mut last_data = Instant::now();
    let mut exited_at: Option<Instant> = None;
    let mut after_exit = 0usize;
    let mut held = false;
    loop {
        let (broken, late) = {
            let q = tee.lock();
            (q.broken, q.late)
        };
        if broken {
            // The consumer went away: stop reading so the read end closes and gh's next write to
            // this stream fails, as it would have written to the consumer directly. After a late
            // signal gh has exited, so what the stream holds is output gh already wrote: scan
            // what can be read at once first, delivering none of it.
            if late {
                take_ready(&mut src, which, &scanner, None, &mut buf);
            }
            break;
        }
        if exited_at.is_none() && tee.child_exited.load(Ordering::SeqCst) {
            exited_at = Some(Instant::now());
            last_data = Instant::now();
        }
        if let Some(at) = exited_at {
            let now = Instant::now();
            if now.duration_since(last_data).as_secs_f64() >= TEE_IDLE_SECS
                || now.duration_since(at).as_secs_f64() >= TEE_AFTER_EXIT_SECS
                || after_exit >= TEE_AFTER_EXIT_BYTES
            {
                // The time since the last read includes any wait in the hook, so gh's own output
                // may still be unread: take what can be read at once before the rest goes to a
                // drainer, which does not scan stdout (or, after a late signal, is dropped).
                take_ready(&mut src, which, &scanner, Some(&tee), &mut buf);
                held = true;
                break;
            }
        }
        if !readable(fd, 100) {
            continue;
        }
        match src.read_chunk(&mut buf) {
            Chunk::End => break,
            // The poll's report was stale; check the cutoffs and poll again.
            Chunk::NotReady => {}
            Chunk::Data(n) => {
                last_data = Instant::now();
                if exited_at.is_some() {
                    after_exit += n;
                }
                scan_and_publish(
                    &scanner,
                    which,
                    Some(&buf[..n]),
                    hook.as_ref(),
                    &mut published,
                );
                tee.push(buf[..n].to_vec());
            }
        }
    }
    let changed = scan(&scanner, which, None, hook.is_some(), &mut published);
    tee.scanned.store(true, Ordering::SeqCst);
    publish(hook.as_ref(), changed);
    tee.close();
    held.then(|| src.into_blocking())
}

/// Feed the scanner what `src` holds that can be read without waiting, up to
/// [`READY_SCAN_BYTES`], and queue it on `deliver` if given. The reads do not wait (see
/// [`ReadEnd`]), so a descendant that keeps writing holds the reader for at most the byte bound
/// and one that is silent not at all. The hook is not called here: the reader feeds the end of
/// the stream next, which calls it once if the signals changed.
fn take_ready(
    src: &mut ReadEnd,
    which: Stream,
    scanner: &Mutex<Scanner>,
    deliver: Option<&Tee>,
    buf: &mut [u8],
) {
    let fd = src.file.as_raw_fd();
    let mut total = 0usize;
    while total < READY_SCAN_BYTES && readable(fd, 0) {
        let room = buf.len().min(READY_SCAN_BYTES - total);
        match src.read_chunk(&mut buf[..room]) {
            Chunk::End | Chunk::NotReady => break,
            Chunk::Data(n) => {
                total += n;
                let _ = scan(scanner, which, Some(&buf[..n]), false, &mut String::new());
                if let Some(tee) = deliver {
                    tee.push(buf[..n].to_vec());
                }
            }
        }
    }
}

/// What one read of gh's stream gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunk {
    /// This many bytes, at the start of the buffer.
    Data(usize),
    /// Nothing can be read now; the stream is still open.
    NotReady,
    /// End of file, EIO from a pseudo-terminal master whose slave is closed, or another error.
    End,
}

/// The reader's end of gh's stream, switched to non-blocking reads for as long as the reader
/// holds it, so a read never waits for output even when the poll before it reported the stream
/// readable and that report is stale by the time of the read.
struct ReadEnd {
    file: File,
    /// The status flags before the switch, restored by [`ReadEnd::into_blocking`]; None if they
    /// could not be read or set, and the reads then block as they did before.
    flags: Option<libc::c_int>,
}

impl ReadEnd {
    /// Switch `file` to non-blocking reads. gh-paced made the stream (a pipe's read end or a
    /// pseudo-terminal master), so no other process shares the flag.
    fn new(file: File) -> ReadEnd {
        let fd = file.as_raw_fd();
        // SAFETY: fcntl on a descriptor `file` owns.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        // SAFETY: as above.
        let set =
            flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0;
        ReadEnd {
            file,
            flags: set.then_some(flags),
        }
    }

    /// Read once, retrying EINTR.
    fn read_chunk(&mut self, buf: &mut [u8]) -> Chunk {
        loop {
            match self.file.read(buf) {
                Ok(0) => return Chunk::End,
                Ok(n) => return Chunk::Data(n),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Chunk::NotReady,
                Err(_) => return Chunk::End,
            }
        }
    }

    /// Give the stream back with its status flags as they were, for the drainer, which reads
    /// its standard input with blocking reads until the last writer closes it.
    fn into_blocking(self) -> File {
        if let Some(flags) = self.flags {
            // SAFETY: fcntl on a descriptor `self.file` owns.
            unsafe {
                libc::fcntl(self.file.as_raw_fd(), libc::F_SETFL, flags);
            }
        }
        self.file
    }
}

/// Copy the queue to our stream until the reader has stopped and the queue is empty. A write
/// error (the consumer went away) marks the queue broken and discards the rest, and the reader
/// then stops and closes gh's stream (see [`tee_reader`]).
fn tee_writer(which: Stream, tee: Arc<Tee>) {
    loop {
        let chunk = {
            let mut q = tee.lock();
            loop {
                if let Some(c) = q.chunks.pop_front() {
                    q.bytes -= c.len();
                    break Some(c);
                }
                if q.closed {
                    break None;
                }
                q = tee
                    .changed
                    .wait(q)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        tee.changed.notify_all();
        let Some(chunk) = chunk else {
            return;
        };
        if !write_fd_all(which.fd(), &chunk) {
            tee.abandon();
        }
    }
}

/// Start the drainer on `src`, a stream a descendant of gh still holds. The drainer gets the
/// stream as its standard input and gh-paced's own standard output and error; nothing else
/// gh-paced holds is inherited (every other descriptor is close-on-exec, the write lease and
/// snapshot lock included). gh-paced does not wait for it. False when there is no drainer or it
/// could not be started; the stream is then closed.
fn hand_off(src: File, which: Stream, drain: Option<&DrainCommand>) -> bool {
    let Some(d) = drain else {
        return false;
    };
    Command::new(&d.program)
        .args(&d.args)
        .arg(which.name())
        .stdin(Stdio::from(src))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .is_ok()
}

/// The drainer (`gh-paced --drain ... STREAM`): copy standard input to `which` (descriptor 2
/// or 1) until the last writer closes it (end of file, or EIO from a pseudo-terminal master) or
/// a write fails because the consumer went away. Closing standard input on the way out makes
/// the descendant's next write fail just as it would have writing to the consumer directly.
/// stderr is fed to the pushback scanner before each chunk is written, calling `hook` when its
/// signals change; stdout is copied unscanned: the reader has already taken what gh wrote (see
/// [`take_ready`]), and a descendant writes no HTTP headers.
pub fn drain(which: Stream, hook: Option<&PushbackHook>) -> i32 {
    let scanner = Mutex::new(Scanner::new());
    let mut published = String::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: reading into a valid buffer of the given length from descriptor 0.
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        let n = match usize::try_from(n) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
        };
        if which == Stream::Err {
            scan_and_publish(&scanner, which, Some(&buf[..n]), hook, &mut published);
        }
        if !write_fd_all(which.fd(), &buf[..n]) {
            break;
        }
    }
    0
}

/// A child stream gh-paced reads: the child's end goes into the Command, the reader is kept.
enum Capture {
    /// Through a pseudo-terminal: the slave is the child's end.
    Pty(Pty),
    /// Through a pipe made by `Command`.
    Pipe,
}

fn capture_for(fd: RawFd) -> Capture {
    if is_tty(fd) {
        if let Some(p) = open_pty(fd) {
            return Capture::Pty(p);
        }
    }
    Capture::Pipe
}

impl Capture {
    fn child_end(&self) -> Stdio {
        match self {
            Capture::Pty(p) => p
                .slave
                .try_clone()
                .map(Stdio::from)
                .unwrap_or_else(|_| Stdio::piped()),
            Capture::Pipe => Stdio::piped(),
        }
    }
}

/// Wait for the child, enforcing the deadline if there is one. Returns the status and whether
/// the deadline stopped it.
fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline_secs: Option<f64>,
) -> std::io::Result<(std::process::ExitStatus, bool)> {
    let Some(limit) = deadline_secs else {
        return child.wait().map(|s| (s, false));
    };
    let start = Instant::now();
    let term_at = start + Duration::from_secs_f64(limit.max(0.0));
    let mut kill_at: Option<Instant> = None;
    loop {
        if let Some(s) = child.try_wait()? {
            return Ok((s, kill_at.is_some()));
        }
        let now = Instant::now();
        match kill_at {
            None if now >= term_at => {
                if let Ok(pid) = i32::try_from(child.id()) {
                    // SAFETY: signalling our own child, which has not been reaped yet.
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                }
                kill_at = Some(now + Duration::from_secs_f64(KILL_GRACE_SECS));
            }
            Some(k) if now >= k => {
                let _ = child.kill();
                return child.wait().map(|s| (s, true));
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn base_command(inv: &Invocation<'_>) -> Command {
    let mut cmd = Command::new(inv.program);
    cmd.args(inv.args);
    for var in ["GH_NO_UPDATE_NOTIFIER", "GH_NO_EXTENSION_UPDATE_NOTIFIER"] {
        if std::env::var_os(var).is_none() {
            cmd.env(var, "1");
        }
    }
    for (k, v) in &inv.env {
        cmd.env(k, v);
    }
    cmd
}

fn feed_stdin(child: &mut std::process::Child, data: Option<Vec<u8>>) {
    if let (Some(mut pipe), Some(bytes)) = (child.stdin.take(), data) {
        std::thread::spawn(move || {
            let _ = pipe.write_all(&bytes);
        });
    }
}

impl Runner for RealRunner {
    fn run(&self, inv: Invocation<'_>, scanner: &mut Scanner) -> Result<Ran, RunError> {
        let shared = Arc::new(Mutex::new(std::mem::take(scanner)));
        let err_cap = capture_for(2);
        let out_cap = if inv.scan_stdout {
            Some(capture_for(1))
        } else {
            None
        };
        let keep_fds = inv.keep_fds.clone();
        let set = signal_set();
        // SAFETY: blocking signals for this thread during spawn; restored below.
        let mut old: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
        }
        LATE_SIGNAL.store(0, Ordering::SeqCst);
        let installed = install_forwarders();
        // The Command (and its copies of the pty slaves) is dropped at the end of this block, so
        // the readers see EOF once the child and its descendants close the streams.
        let spawn_result = {
            let mut cmd = base_command(&inv);
            // The forwarded signals are blocked while spawning, and a child inherits the signal
            // mask. Give the child gh-paced's original mask back, after first resetting the
            // forwarding handlers so a signal that arrives between fork and exec takes its
            // default action in the child instead of running the parent's handler there.
            let child_mask = old;
            // SAFETY: the closure runs in the forked child before exec and only calls
            // async-signal-safe functions (signal, pthread_sigmask, fcntl) on copied data.
            unsafe {
                cmd.pre_exec(move || {
                    for (i, s) in FORWARDED.into_iter().enumerate() {
                        if installed[i] {
                            libc::signal(s, libc::SIG_DFL);
                        }
                    }
                    libc::pthread_sigmask(libc::SIG_SETMASK, &child_mask, std::ptr::null_mut());
                    for &fd in &keep_fds {
                        // Clear close-on-exec in the child only, so gh inherits the lock.
                        if libc::fcntl(fd, libc::F_SETFD, 0) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
            cmd.stdin(if inv.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::inherit()
            });
            match &out_cap {
                Some(c) => cmd.stdout(c.child_end()),
                None => cmd.stdout(Stdio::inherit()),
            };
            cmd.stderr(err_cap.child_end());
            cmd.spawn()
        };
        let mut child = match spawn_result {
            Ok(c) => c,
            Err(e) => {
                // SAFETY: restoring the saved mask.
                unsafe {
                    libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
                }
                // `spawn` also reports a failed exec in the child, so gh's program never ran.
                return Err(RunError {
                    message: format!("cannot run {}: {e}", inv.program.display()),
                    started: false,
                });
            }
        };
        CHILD_PID.store(i32::try_from(child.id()).unwrap_or(0), Ordering::SeqCst);
        // SAFETY: restoring the saved mask; pending signals are now forwarded to the child.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        }
        let err_src: Option<File> = match err_cap {
            Capture::Pipe => child
                .stderr
                .take()
                .map(|e| File::from(std::os::fd::OwnedFd::from(e))),
            Capture::Pty(p) => {
                drop(p.slave);
                Some(p.master)
            }
        };
        let out_src: Option<File> = match out_cap {
            None => None,
            Some(Capture::Pipe) => child
                .stdout
                .take()
                .map(|o| File::from(std::os::fd::OwnedFd::from(o))),
            Some(Capture::Pty(p)) => {
                drop(p.slave);
                Some(p.master)
            }
        };
        let mut tees = Vec::new();
        for (src, which) in [(err_src, Stream::Err), (out_src, Stream::Out)] {
            if let Some(src) = src {
                let tee = Arc::new(Tee::default());
                let (s, t) = (Arc::clone(&shared), Arc::clone(&tee));
                let hook = inv.on_pushback.clone();
                let reader = std::thread::spawn(move || tee_reader(src, which, s, t, hook));
                let t = Arc::clone(&tee);
                let writer = std::thread::spawn(move || tee_writer(which, t));
                tees.push((which, (tee, reader, writer)));
            }
        }
        feed_stdin(&mut child, inv.stdin);
        let status = wait_with_deadline(&mut child, inv.deadline_secs).map_err(|e| RunError {
            message: format!("waiting for {}: {e}", inv.program.display()),
            started: true,
        });
        CHILD_PID.store(0, Ordering::SeqCst);
        // Every reader stops on its own once gh has exited (see the module documentation); every
        // writer then delivers what was read, however long the consumer takes, unless a user
        // signal says to stop.
        for (_, (tee, _, _)) in &tees {
            tee.child_exited();
        }
        let late_signal = loop {
            if tees
                .iter()
                .all(|(_, (_, r, w))| r.is_finished() && w.is_finished())
            {
                break None;
            }
            let sig = LATE_SIGNAL.load(Ordering::SeqCst);
            if sig != 0 {
                for (_, (tee, _, _)) in &tees {
                    tee.abandon_late();
                }
                break Some(sig);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut cut_off = false;
        let teed_stdout = tees.iter().any(|(which, _)| *which == Stream::Out);
        if late_signal.is_none() {
            for (which, (_, reader, writer)) in tees {
                let held = reader.join().ok().flatten();
                let _ = writer.join();
                // The writer has delivered everything read, so the drainer continues in order.
                if let Some(src) = held {
                    cut_off |= !hand_off(src, which, inv.drain.as_ref());
                }
            }
        } else {
            // The readers are not joined, but gh-paced waits for them to scan: one may be in the
            // hook, waiting for the state file's lock to record a cooldown, while gh's last
            // output sits unread in the pipe. Each stops at its next poll now that delivery is
            // abandoned, after scanning what it can read at once and the end of its stream.
            let hook_wait = if inv.hook_wait_secs.is_finite() {
                inv.hook_wait_secs.clamp(0.0, 3600.0)
            } else {
                0.0
            };
            let until = Instant::now() + Duration::from_secs_f64(hook_wait + LATE_SCAN_WAIT_SECS);
            while Instant::now() < until
                && !tees
                    .iter()
                    .all(|(_, (tee, _, _))| tee.scanned.load(Ordering::SeqCst))
            {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if let Ok(s) = shared.lock() {
            *scanner = s.clone();
        }
        if late_signal.is_some() && teed_stdout {
            // If the stdout reader has not got there yet, the end of stdout is fed here, so a
            // header block already read still counts, as one the output cut short (a second
            // end of stdout changes nothing).
            scanner.end_of_stdout();
        }
        status.map(|(s, deadline_hit)| Ran {
            exit: decode(s),
            deadline_hit,
            late_signal,
            cut_off,
        })
    }

    fn capture(&self, inv: Invocation<'_>, timeout_secs: f64) -> Result<Captured, String> {
        let mut cmd = base_command(&inv);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot run {}: {e}", inv.program.display()))?;
        let mut out = child.stdout.take();
        let mut err = child.stderr.take();
        // One byte past the limit is read, so a stream that had more is known to be cut.
        fn read_capped(stream: Option<&mut impl Read>) -> (Vec<u8>, bool) {
            let mut v = Vec::new();
            if let Some(s) = stream {
                let _ = s.take(MAX_CAPTURE_BYTES as u64 + 1).read_to_end(&mut v);
            }
            let cut = v.len() > MAX_CAPTURE_BYTES;
            v.truncate(MAX_CAPTURE_BYTES);
            (v, cut)
        }
        let out_t = std::thread::spawn(move || read_capped(out.as_mut()));
        let err_t = std::thread::spawn(move || read_capped(err.as_mut()));
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs.max(0.1));
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() >= deadline => {
                    timed_out = true;
                    let _ = child.kill();
                    break child
                        .wait()
                        .map_err(|e| format!("waiting for {}: {e}", inv.program.display()))?;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(e) => return Err(format!("waiting for {}: {e}", inv.program.display())),
            }
        };
        let (stdout, stdout_cut) = out_t.join().unwrap_or_default();
        let (stderr, stderr_cut) = err_t.join().unwrap_or_default();
        Ok(Captured {
            exit: decode(status),
            stdout,
            stderr,
            timed_out,
            stdout_cut,
            stderr_cut,
        })
    }
}

/// True when gh-paced's stdin is a terminal.
pub fn stdin_is_tty() -> bool {
    // SAFETY: isatty on a standard descriptor.
    unsafe { libc::isatty(std::io::stdin().as_raw_fd()) == 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::mpsc;

    /// A pseudo-terminal: the master, the slave, and the slave's path.
    fn pty_with_path() -> (File, File, std::path::PathBuf) {
        let p = open_pty(-1).expect("a pseudo-terminal");
        let mut name = [0 as libc::c_char; 128];
        // SAFETY: ptsname_r on a master we own, into a stack buffer of the given length.
        let rc = unsafe { libc::ptsname_r(p.master.as_raw_fd(), name.as_mut_ptr(), name.len()) };
        assert_eq!(rc, 0);
        // SAFETY: ptsname_r succeeded, so the buffer holds a NUL-terminated name.
        let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
        (p.master, p.slave, path.to_str().unwrap().into())
    }

    fn status_flags(fd: RawFd) -> libc::c_int {
        // SAFETY: F_GETFL on a descriptor the caller owns.
        unsafe { libc::fcntl(fd, libc::F_GETFL) }
    }

    /// The race a blocking read lost: the last slave descriptor closes, so the master reports a
    /// hang-up; a descendant of gh opens the terminal again before the reader's read, and writes
    /// nothing. The read must say that nothing is ready instead of waiting for the next write,
    /// which would keep the reader past every after-exit cutoff.
    #[test]
    fn a_read_after_a_stale_hang_up_report_does_not_wait() {
        let (master, slave, path) = pty_with_path();
        let fd = master.as_raw_fd();
        let mut end = ReadEnd::new(master);
        drop(slave);
        assert!(
            readable(fd, 0),
            "a closed slave makes the master report a hang-up"
        );
        let again = File::options()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(&path)
            .unwrap();
        assert!(
            !readable(fd, 0),
            "the slave is open again and nothing was written"
        );
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let _ = tx.send(end.read_chunk(&mut buf));
        });
        let got = rx.recv_timeout(Duration::from_secs(2));
        if got.is_err() {
            // The read is waiting: write to end it, so the test fails instead of hanging.
            let _ = (&again).write_all(b"x");
        }
        let _ = reader.join();
        assert_eq!(got, Ok(Chunk::NotReady));
    }

    /// The reader gives a stream it hands to the drainer back with its flags as they were: the
    /// drainer's reads block until the last writer closes the stream, and a non-blocking one
    /// would end the copy at the first pause.
    #[test]
    fn a_stream_handed_on_reads_blocking_again() {
        let (master, _slave, _) = pty_with_path();
        let fd = master.as_raw_fd();
        let before = status_flags(fd);
        assert_eq!(before & libc::O_NONBLOCK, 0);
        let end = ReadEnd::new(master);
        assert_ne!(status_flags(fd) & libc::O_NONBLOCK, 0);
        let back = end.into_blocking();
        assert_eq!(status_flags(back.as_raw_fd()), before);
    }
}
