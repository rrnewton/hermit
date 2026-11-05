//! The checkpointer: a thread with a connection of its own that copies the write-ahead log back
//! into the database while the writer goes on writing.
//!
//! SQLite's automatic checkpoint runs inside whichever commit takes the log past a thousand
//! pages, and copying four megabytes and syncing the database held that commit, and every
//! barrier behind it, for up to seconds on a loaded host. The writer turns it off. This thread
//! runs a `PASSIVE` checkpoint whenever the log passes [`PASSIVE_BYTES`], concurrently with the
//! writer: a passive checkpoint never takes the write lock.
//!
//! It cannot finish the job alone. A log is restarted from its beginning only by a write that
//! finds every frame copied back, and a writer that never pauses always has frames newer than
//! the checkpoint that just ended. So the writer itself checkpoints between its steps once the
//! log has grown past a larger bound, copying only what this thread has not, and it is the
//! writer that truncates the log after deleting text.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};

use super::schema::{apply_per_open, sqlite_code};
use super::{file, lock, CacheError, Shared, Stats};

/// A passive checkpoint runs once the log is larger than this.
pub(crate) const PASSIVE_BYTES: u64 = 4 * 1024 * 1024;
/// How often the thread looks at the log.
const LOOK_EVERY: Duration = Duration::from_millis(100);

#[derive(Debug, Default)]
struct Want {
    stop: bool,
    /// Report this connection's pragmas, for the per-open assertion.
    #[cfg(test)]
    probe: Option<std::sync::mpsc::Sender<Vec<(String, String)>>>,
}

#[derive(Debug, Default)]
struct State {
    want: Mutex<Want>,
    changed: Condvar,
}

/// The running checkpointer. Dropping it stops the thread and closes its connection.
pub(crate) struct Checkpointer {
    state: Arc<State>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Checkpointer {
    /// Open a connection to the cache at `path` and start the thread.
    ///
    /// # Errors
    ///
    /// When the connection cannot be opened or set up, or the thread cannot start.
    pub(crate) fn start(path: &Path, shared: Arc<Shared>) -> Result<Self, CacheError> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| CacheError::Sqlite(sqlite_code(&e)))?;
        apply_per_open(&connection).map_err(|e| CacheError::Sqlite(sqlite_code(&e)))?;
        let state = Arc::new(State::default());
        let thread_state = Arc::clone(&state);
        let wal = file::family(path)[1].clone();
        let handle = std::thread::Builder::new()
            .name("message-cache-checkpointer".to_owned())
            .spawn(move || run(&connection, &thread_state, &shared, &wal))
            .map_err(|e| {
                CacheError::Filesystem(format!("cannot start the cache checkpointer: {}", e.kind()))
            })?;
        Ok(Self {
            state,
            handle: Some(handle),
        })
    }

    /// This connection's pragmas.
    #[cfg(test)]
    pub(crate) fn probe(&self) -> Vec<(String, String)> {
        let (reply, answer) = std::sync::mpsc::channel();
        lock(&self.state.want).probe = Some(reply);
        self.state.changed.notify_all();
        answer
            .recv_timeout(Duration::from_secs(10))
            .expect("the checkpointer answered")
    }
}

impl Drop for Checkpointer {
    fn drop(&mut self) {
        lock(&self.state.want).stop = true;
        self.state.changed.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(connection: &Connection, state: &State, shared: &Shared, wal: &PathBuf) {
    loop {
        {
            let mut want = lock(&state.want);
            if !want.stop {
                want = state
                    .changed
                    .wait_timeout(want, LOOK_EVERY)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            if want.stop {
                return;
            }
            #[cfg(test)]
            if let Some(reply) = want.probe.take() {
                let _ = reply.send(super::probe(connection));
            }
        }
        if !std::fs::metadata(wal).is_ok_and(|m| m.len() > PASSIVE_BYTES) {
            continue;
        }
        let started = Instant::now();
        match connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
        }) {
            Ok((log, copied)) => shared
                .log_backlog
                .store((log - copied).max(0), Ordering::SeqCst),
            Err(error) => {
                tracing::warn!(code = %sqlite_code(&error), "a message cache checkpoint failed");
            }
        }
        Stats::max(
            &shared.stats.checkpoint_max_us,
            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        );
    }
}
