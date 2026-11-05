//! The background poller: one process per cache directory, polling every interval so the history
//! has the samples burn rates need.

use crate::history::Lock;
use crate::model::PROVIDERS;
use crate::paths::{ensure_dir, CachePaths, Env};
use crate::poll::{step, PollOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Default polling interval: 15 minutes.
pub const DEFAULT_INTERVAL: i64 = 900;
/// Shortest interval accepted.
pub const MIN_INTERVAL: i64 = 60;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_handlers() {
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The running daemon's pid, if one holds the daemon lock.
pub fn running_pid(paths: &CachePaths) -> Option<i32> {
    if !paths.daemon_lock().exists() {
        return None;
    }
    match Lock::try_acquire(&paths.daemon_lock()) {
        Ok(Some(_free)) => None,
        Ok(None) => std::fs::read_to_string(paths.daemon_pid())
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .or(Some(0)),
        Err(_) => None,
    }
}

/// Run in the foreground until signalled. With `once`, poll a single time and return.
pub fn run(paths: &CachePaths, env: Env, interval: i64, once: bool) -> Result<i32, String> {
    ensure_dir(&paths.dir)?;
    let Some(mut held) = Lock::try_acquire(&paths.daemon_lock())? else {
        let pid = running_pid(paths).unwrap_or(0);
        eprintln!(
            "agent-usage daemon: already running (pid {pid}) for {}",
            paths.dir.display()
        );
        return Ok(75);
    };
    let pid = std::process::id();
    let _ = held.file().set_len(0);
    let _ = writeln!(held.file(), "{pid}");
    std::fs::write(paths.daemon_pid(), format!("{pid}\n"))
        .map_err(|e| format!("write pid file: {e}"))?;
    install_handlers();
    eprintln!(
        "agent-usage daemon: pid {pid}, polling every {interval}s into {}",
        paths.dir.display()
    );
    let opts = PollOptions {
        providers: PROVIDERS.to_vec(),
        max_age: Some(0),
        scan_transcripts: true,
    };
    loop {
        let t = now();
        match step(paths, env, &opts, t) {
            Ok(result) => {
                let summary: Vec<String> = result
                    .samples
                    .iter()
                    .rev()
                    .filter(|s| s.ts == t)
                    .map(|s| format!("{}={:?}/{}ms", s.provider, s.status, s.elapsed_ms))
                    .collect();
                eprintln!(
                    "{} poll {}",
                    crate::timefmt::rfc3339_utc(t),
                    summary.join(" ")
                );
            }
            Err(e) => eprintln!("{} poll failed: {e}", crate::timefmt::rfc3339_utc(t)),
        }
        if once {
            break;
        }
        // Sleep in short naps so a signal stops the daemon promptly.
        let wake = t + interval;
        while !STOP.load(Ordering::SeqCst) && now() < wake {
            std::thread::sleep(Duration::from_millis(500));
        }
        if STOP.load(Ordering::SeqCst) {
            break;
        }
    }
    let _ = std::fs::remove_file(paths.daemon_pid());
    eprintln!("agent-usage daemon: pid {pid} stopped");
    Ok(0)
}

/// Start a detached daemon (new session, output to `daemon.log`) and return its pid.
pub fn detach(paths: &CachePaths, exe: &Path, interval: i64) -> Result<u32, String> {
    ensure_dir(&paths.dir)?;
    if let Some(pid) = running_pid(paths) {
        return Err(format!("already running (pid {pid})"));
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.daemon_log())
        .map_err(|e| format!("open {}: {e}", paths.daemon_log().display()))?;
    let log2 = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.args(["daemon", "--interval", &interval.to_string()])
        .env("AGENT_USAGE_DIR", &paths.dir)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log2);
    // SAFETY: setsid is async-signal-safe and is the only call made in the child before exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| format!("spawn daemon: {e}"))?;
    Ok(child.id())
}

/// Stop the running daemon with SIGTERM. Returns its pid, or `None` when none runs.
pub fn stop(paths: &CachePaths) -> Result<Option<i32>, String> {
    match running_pid(paths) {
        None => Ok(None),
        Some(0) => Err("a daemon holds the lock but its pid file is missing".into()),
        Some(pid) => {
            // SAFETY: plain kill(2).
            if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
                return Err(format!("kill {pid}: {}", std::io::Error::last_os_error()));
            }
            Ok(Some(pid))
        }
    }
}
