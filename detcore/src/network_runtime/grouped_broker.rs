//! Retained grouped-startup prerequisites. No provider-open or global-cleanup
//! capability can be constructed from this module's serialized diagnostics.
//!
//! Create the recovery owner outside any cancellable future. The infallible
//! retain operation moves every original resource before any I/O. Operation
//! handles hold the same state; dropping an operation or returning Err leaves
//! the recovery owner holding partial children, received rights and journals.
//! Calls are synchronous and check the original absolute deadline. Durable
//! filesystem I/O may block; enclosing process supervision remains required.
//! After durability, deadline checks precede ACKs and C rechecks before effects.
mod adoption;
mod cleanup;
mod cleanup_native;
mod entry;
mod guardian;
mod journal;
mod keeper;
mod native;
mod parent_launch;
mod runtime_cleanup;
mod runtime_keeper;
mod serial;
mod source_owner;

pub use entry::run_grouped_leaf_delegate_process;
pub use entry::run_grouped_source_process;
pub use entry::run_grouped_startup_controller_process;
pub(super) use native::Bridge;
pub(super) use runtime_cleanup::RuntimeCleanup;
pub use runtime_keeper::run_grouped_runtime_keeper_process;
pub use source_owner::run_grouped_source_owner_process;
mod owner;
pub use owner::GroupedParentOwner;
#[cfg(test)]
mod tests;
mod wire;

use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::process::Child;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;

fn require(ok: bool, message: &str) -> io::Result<()> {
    if ok {
        Ok(())
    } else {
        Err(io::Error::other(message.to_owned()))
    }
}
fn query_admission(
    creator: bool,
    query: Option<usize>,
    queries: usize,
    entry_query: Option<usize>,
    entry_queries: usize,
) -> io::Result<()> {
    // Original creator/overlap/128-receipt guard, before any query allocation or spawn.
    require(
        creator && query.is_none() && queries < 128,
        "manager query lacks retained creator or overlaps another query",
    )?;
    require(
        entry_query.is_none() && entry_queries < 128,
        "entry query overlaps or exceeds original128 bound",
    )
}
fn send_after_durability(
    channel: &mut wire::Channel,
    deadline: Instant,
    packet: &[u8],
) -> io::Result<()> {
    require(
        Instant::now() < deadline,
        "durable ACK exceeded original absolute deadline",
    )?;
    channel.send_once(packet, &[])
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn valid_nonce(nonce: &str) -> bool {
    nonce.len() == 32
        && nonce
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && nonce.bytes().any(|b| b != b'0')
}
#[derive(Clone, Debug)]
struct Failure {
    kind: io::ErrorKind,
    errno: Option<i32>,
    message: String,
}
impl Failure {
    fn capture(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            errno: error.raw_os_error(),
            message: error.to_string(),
        }
    }
    fn error(&self) -> io::Error {
        self.errno
            .map(io::Error::from_raw_os_error)
            .unwrap_or_else(|| io::Error::new(self.kind, self.message.clone()))
    }
}
const SITES: [(&str, u32); 17] = [
    ("__sys_connect", 0x1c),
    ("__sys_connect", 0x41),
    ("__sys_connect", 0x46),
    ("__sys_accept4", 0x21),
    ("fdget_raw", 0x7c),
    ("fdget_raw", 0x5),
    ("__x64_sys_read", 0x13),
    ("fdget_pos", 0x96),
    ("fdget_pos", 0xfa),
    ("do_epoll_ctl", 0x23),
    ("do_epoll_ctl", 0x37),
    ("__skb_datagram_iter", 0x64),
    ("__skb_datagram_iter", 0x69),
    ("__skb_datagram_iter", 0x26b),
    ("__skb_datagram_iter", 0x270),
    ("inet_recvmsg", 0x1b),
    ("inet6_recvmsg", 0x1b),
];
#[derive(Clone, Debug)]
pub(super) struct Intent {
    nonce: String,
    incarnation: u64,
}
impl Intent {
    pub fn new(nonce: String, incarnation: u64) -> io::Result<Self> {
        require(
            valid_nonce(&nonce) && incarnation != 0,
            "invalid grouped intent",
        )?;
        Ok(Self { nonce, incarnation })
    }
    fn group(&self) -> String {
        format!("hermit_{}", self.nonce)
    }
    fn event(&self) -> String {
        format!("hermit_classic_{}", self.nonce)
    }
    fn command(&self, role: u32, remove: u32) -> io::Result<String> {
        require(
            (1..=17).contains(&role) && remove <= 1,
            "invalid fixed grouped site role",
        )?;
        let (symbol, offset) = SITES[role as usize - 1];
        Ok(format!(
            "{}:{}/{} {symbol}+{offset}\n",
            if remove == 1 { '-' } else { 'p' },
            self.group(),
            self.event()
        ))
    }
}
#[derive(Debug)]
struct State {
    intent: Intent,
    unit: String,
    deadline: Instant,
    channel: wire::Channel,
    launcher: owner::Launcher,
    journal: journal::Journal,
    directory: OwnedFd,
    creator: Option<owner::Creator>,
    controls: Option<owner::Controls>,
    entries: Vec<owner::EntryImage>,
    entry_queries: Vec<owner::EntryQuery>,
    entry_query: Option<usize>,
    entry_snapshot: Option<owner::EntrySnapshot>,
    manager_snapshot: Option<owner::ManagerSnapshot>,
    role_queries: Vec<owner::RoleQuery>,
    role_query: Option<usize>,
    queries: Vec<owner::ManagerQuery>,
    query: Option<usize>,
    initialized: bool,
    creator_ack_attempted: bool,
    source_eof: bool,
    failure: Option<Failure>,
    release_origin: Option<Instant>,
}
/// This owner belongs to the controller's recovery scope, outside the operation
/// future. No Drop implementation turns open resources into terminal success.
#[derive(Debug)]
#[must_use = "retain outside cancellable operations until actual joined cleanup"]
pub(super) struct RecoveryOwner {
    state: Arc<Mutex<State>>,
}
#[derive(Debug)]
pub(super) struct Operations {
    state: Arc<Mutex<State>>,
}
impl RecoveryOwner {
    /// Infallible transfer. Even initialization refusal leaves every original
    /// FD/Child in this state. `directory` and `journal_directory` are separately
    /// owned actual 0700 handles; no pathname or numeric descriptor is reopened.
    pub fn retain(
        intent: Intent,
        unit: String,
        deadline: Instant,
        channel: OwnedFd,
        launcher: Child,
        directory: OwnedFd,
        journal_directory: OwnedFd,
    ) -> (Self, Operations) {
        let state = Arc::new(Mutex::new(State {
            journal: journal::Journal::retain(journal_directory, intent.clone()),
            intent,
            unit,
            deadline,
            channel: wire::Channel::retain(channel),
            launcher: owner::Launcher::retain(launcher),
            directory,
            creator: None,
            controls: None,
            entries: Vec::new(),
            entry_queries: Vec::new(),
            entry_query: None,
            entry_snapshot: None,
            manager_snapshot: None,
            role_queries: Vec::new(),
            role_query: None,
            queries: Vec::new(),
            query: None,
            initialized: false,
            creator_ack_attempted: false,
            source_eof: false,
            failure: None,
            release_origin: None,
        }));
        (
            Self {
                state: state.clone(),
            },
            Operations { state },
        )
    }
    /// Transfer the actual expected image before fallible hashing and before
    /// source admission. Repeated/failed transfers stay in the external owner.
    pub fn retain_entry(
        &self,
        file: OwnedFd,
        arguments: Vec<std::ffi::OsString>,
    ) -> io::Result<()> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| io::Error::other("grouped recovery state poisoned"))?;
        s.entries.push(owner::EntryImage::retain(file, arguments));
        let result = (|| {
            require(
                s.entries.len() == 1 && !s.initialized,
                "entry must be retained once before initialization",
            )?;
            s.entries[0].initialize()
        })();
        if let Err(error) = &result {
            s.failure.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    /// Read-only diagnostics. Counts/booleans here never issue adoption, deletion,
    /// successor or provider-open authority.
    pub fn diagnostics(&self) -> io::Result<Value> {
        let s = self
            .state
            .lock()
            .map_err(|_| io::Error::other("grouped recovery state poisoned; owner retained"))?;
        Ok(
            json!({"initialized":s.initialized,"creator_present":s.creator.is_some(),
            "creator_admitted":s.creator.as_ref().is_some_and(|c|c.admitted),
            "launcher_pid":s.launcher.child.id(),"launcher_pidfd_held":s.launcher.pidfd.is_some(),
            "received_packets":s.channel.packets.len(),
            "received_aliases":s.channel.packets.iter().map(|p|p.rights.len()).sum::<usize>(),
            "journal_file_held":s.journal.store.file.is_some(),"journal_pairs":s.journal.pairs.len(),
            "entry_queries":s.entry_queries.iter().map(owner::EntryQuery::evidence).collect::<Vec<_>>(),
            "role_queries":s.role_queries.iter().map(owner::RoleQuery::evidence).collect::<Vec<_>>(),
            "failure":s.failure.as_ref().map(|e|e.message.as_str()),"source_eof":s.source_eof,
            "release_started":s.release_origin.is_some()}),
        )
    }
    /// Latch the original before-all-release origin exactly once. This does not
    /// close an FD, signal a task, delete a probe, or assert global absence.
    pub fn begin_release(&self, origin: Instant) -> io::Result<()> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| io::Error::other("grouped recovery state poisoned"))?;
        require(
            s.release_origin.is_none(),
            "original release origin cannot restart",
        )?;
        s.release_origin = Some(origin); // retain even an invalid origin
        require(
            Instant::now() >= origin
                && Instant::now().duration_since(origin) < Duration::from_secs(1),
            "original before-all-release cutoff expired",
        )
    }
}
impl Operations {
    fn step<T>(&self, operation: impl FnOnce(&mut State) -> io::Result<T>) -> io::Result<T> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| io::Error::other("grouped state poisoned; recovery owner retained"))?;
        if let Some(error) = &s.failure {
            return Err(error.error());
        }
        let result = (|| {
            require(
                Instant::now() < s.deadline && s.release_origin.is_none(),
                "original grouped stage deadline expired or releasing",
            )?;
            operation(&mut s)
        })();
        if let Err(error) = &result {
            s.failure.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    pub fn initialize(&self, header: Value) -> io::Result<()> {
        self.step(|s| {
            require(!s.initialized, "grouped owner cannot initialize twice")?;
            require(
                s.deadline.saturating_duration_since(Instant::now()) <= Duration::from_secs(20),
                "original stage allowance exceeds20s",
            )?;
            owner::protected_holder()?;
            let nonce = s
                .unit
                .strip_prefix("hermit-accepted-")
                .and_then(|s| s.strip_suffix(".service"));
            require(
                nonce.is_some_and(valid_nonce),
                "grouped source unit is not the fixed accepted purpose",
            )?;
            let identity = owner::stat(s.directory.as_raw_fd())?;
            require(
                identity.mode & libc::S_IFMT == libc::S_IFDIR
                    && identity.mode & 0o7777 == 0o700
                    && identity.uid == unsafe { libc::getuid() },
                "launcher logs require an actual owned0700 directory",
            )?;
            s.channel.validate()?;
            s.launcher.initialize(s.directory.as_raw_fd())?;
            s.journal.initialize(header)?;
            s.initialized = true;
            Ok(())
        })
    }
    pub fn receive_creator(&self) -> io::Result<bool> {
        self.step(|s| {
            require(
                s.initialized && s.creator.is_none(),
                "creator receipt outside initial stage",
            )?;
            let Some(index) = s.channel.receive(512)? else {
                return Ok(false);
            };
            let creator =
                owner::Creator::retain(&mut s.channel.packets[index], &s.intent, &s.unit)?;
            s.creator = Some(creator); // actual capabilities retained before manager query
            s.journal.store.append(
                json!({"kind":"creator-receipt","created":s.creator.as_ref().unwrap().receipt()}),
            )?;
            Ok(true)
        })
    }
    pub fn begin_manager_query(&self) -> io::Result<()> {
        self.step(|s| {
            query_admission(
                s.creator.is_some(),
                s.query,
                s.queries.len(),
                s.entry_query,
                s.entry_queries.len(),
            )?;
            let creator = s
                .creator
                .as_ref()
                .ok_or_else(|| io::Error::other("creator owner absent"))?;
            s.manager_snapshot = None;
            s.entry_snapshot = None;
            if !creator.admitted {
                require(
                    s.entries.len() == 1,
                    "actual expected creator entry is absent",
                )?;
                owner::pidfd_matches(creator.pidfd.as_raw_fd(), creator.peer.pid)?;
                let index = s.entry_queries.len();
                s.entry_queries
                    .push(owner::EntryQuery::retain(creator.peer.pid));
                s.entry_query = Some(index); // retained before any child can exist
            }
            let index = s.queries.len();
            s.queries.push(owner::ManagerQuery::retain(s.unit.clone()));
            s.query = Some(index); // both query owners exist before either spawn
            s.queries[index].start()?;
            if let Some(index) = s.entry_query {
                s.entry_queries[index].start()?;
            }
            Ok(())
        })
    }
    pub fn poll_creator_authentication(&self) -> io::Result<bool> {
        self.step(|s| {
            let index = s
                .query
                .ok_or_else(|| io::Error::other("manager query absent"))?;
            if s.manager_snapshot.is_none() {
                s.manager_snapshot = s.queries[index].poll(s.deadline)?;
            }
            if let Some(index) = s.entry_query {
                let creator = s
                    .creator
                    .as_ref()
                    .ok_or_else(|| io::Error::other("creator owner absent"))?;
                owner::pidfd_matches(creator.pidfd.as_raw_fd(), creator.peer.pid)?;
                if s.entry_snapshot.is_none() {
                    s.entry_snapshot = s.entry_queries[index].poll(s.deadline)?;
                }
                if s.entry_snapshot.is_none() {
                    return Ok(false);
                }
            }
            let Some(snapshot) = &s.manager_snapshot else {
                return Ok(false);
            };
            let creator = s
                .creator
                .as_mut()
                .ok_or_else(|| io::Error::other("creator owner absent"))?;
            if creator.admitted {
                creator.check_snapshot(snapshot, false)?;
            } else {
                let entry = s
                    .entries
                    .first()
                    .ok_or_else(|| io::Error::other("expected entry owner absent"))?;
                let actual = s
                    .entry_snapshot
                    .as_ref()
                    .ok_or_else(|| io::Error::other("actual executable query absent"))?;
                s.journal.store.append(json!({"kind":"actual-executable-query","observation":s.entry_queries[s.entry_query.unwrap()].evidence()}))?;
                creator.authenticate(snapshot, entry, actual)?;
            }
            s.query = None;
            s.entry_query = None;
            s.journal
                .store
                .append(json!({"kind":"creator-authenticated","owner":creator.evidence()?}))?;
            require(
                Instant::now() < s.deadline,
                "creator authentication durability exceeded original deadline",
            )?;
            Ok(true)
        })
    }
    pub fn acknowledge_creator(&self) -> io::Result<()> {
        self.step(|s| {
            let creator = s.creator.as_ref().ok_or_else(||io::Error::other("creator owner absent"))?;
            require(creator.admitted && !owner::terminal(creator.pidfd.as_raw_fd())?
                && !s.creator_ack_attempted, "creator ACK requires actual live custody and one unused attempt")?;
            s.creator_ack_attempted = true;
            let packet = format!("EXEC {}\n", s.intent.nonce);
            s.journal.store.append(json!({"kind":"creator-ack-intent","packet":hex(packet.as_bytes())}))?;
            require(Instant::now()<s.deadline,"creator ACK durability exceeded original deadline")?;
            creator.process_policy()?;
            require(Instant::now()<s.deadline,"creator ACK policy readback exceeded original deadline")?;
            let result = send_after_durability(&mut s.channel,s.deadline,packet.as_bytes());
            let raw = s.channel.sends.last().and_then(|s|s.raw);
            s.journal.store.append(json!({"kind":"creator-ack-outcome","raw":raw.map(|r|r.returned),"errno":raw.and_then(|r|r.errno)}))?;
            result
        })
    }
    pub fn receive_controls(&self) -> io::Result<bool> {
        self.step(|s| {
            require(
                s.creator_ack_attempted,
                "control receipt outside creator stage",
            )?;
            let creator = s
                .creator
                .as_ref()
                .ok_or_else(|| io::Error::other("creator owner absent"))?;
            if s.controls.is_none() {
                let Some(index) = s.channel.receive(2048)? else {
                    return Ok(false);
                };
                s.controls = Some(owner::Controls::retain(
                    &mut s.channel.packets[index],
                    creator,
                    &s.intent,
                )?);
                let index = s.role_queries.len();
                s.role_queries.push(owner::RoleQuery::retain());
                s.role_query = Some(index); // retained before privileged fixed-path read
                s.role_queries[index].start()?;
            }
            let index = s.role_query.ok_or_else(|| {
                io::Error::other("control roles already admitted or query absent")
            })?;
            let Some(named) = s.role_queries[index].poll(s.deadline)? else {
                return Ok(false);
            };
            owner::pidfd_matches(creator.pidfd.as_raw_fd(), creator.peer.pid)?;
            s.journal.store.append(json!({"kind":"independent-fixed-role-query","observation":s.role_queries[index].evidence()}))?;
            s.controls.as_mut().unwrap().authenticate(&named)?;
            s.role_query = None;
            let held = s.controls.as_ref().unwrap().evidence()?;
            s.journal.store.append(
                json!({"kind":"actual-controls-held","identities":held["identities"],
                "description_matrix":held["description_matrix"],"owner":creator.evidence()?}),
            )?;
            require(
                Instant::now() < s.deadline,
                "control durability exceeded original deadline",
            )?;
            Ok(true)
        })
    }
    pub fn receive_journal(&self) -> io::Result<bool> {
        self.step(|s| {
            let controls = s.controls.as_ref().ok_or_else(||io::Error::other("journal lacks actual tracefs controls"))?;
            controls.check()?;
            let creator = s.creator.as_ref().ok_or_else(||io::Error::other("journal lacks retained creator"))?;
            require(!owner::terminal(creator.pidfd.as_raw_fd())?, "journal creator is terminal")?;
            let Some(index) = s.channel.receive(1536)? else { return Ok(false); };
            let packet = &s.channel.packets[index];
            s.journal.store.append(json!({"kind":"received-callback","flags":packet.flags,"packet":hex(&packet.bytes)}))?;
            packet.exact(0, creator.peer)?;
            let state = creator.readback()?;
            require(!state.creator_terminal && state.procs.as_ref().is_some_and(|p|
                p.lines().any(|line|line == creator.peer.pid.to_string())), "callback creator left retained live cgroup")?;
            let ack = s.journal.receive(&packet.bytes, Instant::now())?;
            require(Instant::now()<s.deadline,"journal durability exceeded original ACK deadline")?;
            require(!owner::terminal(creator.pidfd.as_raw_fd())?,"journal creator became terminal before ACK")?;
            send_after_durability(&mut s.channel,s.deadline,&ack.bytes)?;
            require(!ack.native_failed, "failed native outcome durably retained; creation remains UNKNOWN")?;
            Ok(true)
        })
    }
    pub fn receive_source_eof(&self) -> io::Result<bool> {
        self.step(|s| {
            require(!s.source_eof, "source EOF cannot be reused")?;
            s.journal.complete()?;
            s.controls.as_ref().ok_or_else(||io::Error::other("source EOF lacks controls"))?.check()?;
            let Some(index) = s.channel.receive(1536)? else { return Ok(false); };
            let packet = &s.channel.packets[index];
            s.journal.store.append(json!({"kind":"source-channel-terminal-read","packet":hex(&packet.bytes),
                "flags":packet.flags,"ancillary_count":packet.credentials.len()+packet.rights_messages}))?;
            require(packet.raw.returned == 0 && packet.bytes.is_empty() && packet.credentials.is_empty()
                && packet.rights.is_empty() && packet.rights_messages == 0 && packet.flags == libc::MSG_CMSG_CLOEXEC,
                "source sent bytes/ancillary after exact history")?;
            s.source_eof = true; Ok(true)
        })
    }
    pub fn drain_launcher(&self) -> io::Result<()> {
        self.step(|s| s.launcher.drain())
    }
    /// Diagnostic terminal join only. It deliberately issues no LeafPlan until
    /// the independent guardian's actual journal and endpoint join is wired.
    pub fn check_source_terminal(&self) -> io::Result<()> {
        self.step(|s| {
            require(
                s.source_eof && s.query.is_none(),
                "source channel or manager query unfinished",
            )?;
            s.journal.complete()?;
            let creator = s
                .creator
                .as_ref()
                .ok_or_else(|| io::Error::other("source creator absent"))?;
            let state = creator.readback()?;
            require(
                state.creator_terminal && state.unlinked,
                "source requires terminal creator and positively unlinked retained cgroup",
            )?;
            s.launcher.reap_success(s.directory.as_raw_fd())?;
            Ok(())
        })
    }
}
