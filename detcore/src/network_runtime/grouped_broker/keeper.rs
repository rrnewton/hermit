//! Finite early keeper entry. The source socketpair is created in this actual
//! keeper process, so the source's SO_PEERCRED identifies this endpoint owner.
//! The parent retains its Child; this object retains every received original
//! capability before any fallible interpretation or source handoff.
use std::ffi::OsString;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use super::Failure;
use super::Intent;
use super::guardian;
use super::journal;
use super::owner;
use super::require;
use super::wire;
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    schema: String,
    nonce: String,
    incarnation: u64,
    unit: String,
    stage_deadline: u64,
    arguments: Vec<String>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Launched {
    schema: String,
    nonce: String,
    incarnation: u64,
    stage_deadline: u64,
    wrapper: i32,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CancelBeforeSource {
    schema: String,
    nonce: String,
    incarnation: u64,
    stage_deadline: u64,
    native_origin: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Retained,
    Configuration,
    Launched,
    Source,
}
#[derive(Debug)]
#[must_use = "keeper custody persists through ambiguous transfer or parent loss"]
pub(super) struct Keeper {
    parent: wire::Channel,
    parent_credentials: Option<wire::Credentials>,
    parent_pidfd: Option<OwnedFd>,
    holder: Option<guardian::Holder>,
    source_endpoint: Option<OwnedFd>,
    guardian_endpoint: Option<OwnedFd>,
    wrapper_pidfd: Option<OwnedFd>,
    configuration: Option<Configuration>,
    deadline: Instant,
    native_deadline: u64,
    stage: Stage,
    refused: Option<Failure>,
    closed_guardian: bool,
    failed_report_attempted: bool,
    startup_cancellation: Option<CancelBeforeSource>,
    startup_report_attempted: bool,
    configuration_retirement: Option<ConfigurationRetirement>,
    configuration_failure_origin: Option<u64>,
    passcred_attempts: [Option<PasscredAttempt>; 2],
    rejected_startup: Option<RejectedStartup>,
    uncaptured_startup: Option<RejectedStartup>,
    successful_exit_attempted: bool,
    successful_exit_sent: bool,
    terminal_export: Option<super::adoption::TerminalExport>,
    creation_cleanup: Option<super::cleanup::CreationPeer>,
    // Optional only for the new closed production entry. Old callers and all
    // original configuration/cancellation identities remain unchanged.
    source_bridge: Option<SourceBridge>,
    mirrored_prefix: Option<(u64, serde_json::Value)>,
    initial_prefix_attempted: bool,
}
#[derive(Debug)]
struct SourceBridge {
    configuration: super::entry::BridgeConfiguration,
    file: OwnedFd,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RejectedPhase {
    Queries,
    Revoking,
    Source,
    Complete,
}
#[derive(Debug)]
struct RejectedStartup {
    phase: RejectedPhase,
    failure: Option<Failure>,
    query_observation: Option<serde_json::Value>,
    holder_observation: Option<serde_json::Value>,
    guardian_close: Option<serde_json::Value>,
    report_attempted: bool,
    agreement_attempted: bool,
    agreement_acknowledged: bool,
}
#[derive(Debug)]
struct PasscredAttempt {
    fd: i32,
    raw: Option<wire::RawCall>,
}
impl PasscredAttempt {
    fn observation(&self) -> serde_json::Value {
        json!({"fd":self.fd,"level":libc::SOL_SOCKET,"option":libc::SO_PASSCRED,
            "value":1,"bytes":4,"returned":self.raw.map(|r|r.returned),"errno":self.raw.and_then(|r|r.errno)})
    }
}
#[derive(Debug)]
struct ConfigurationRetirement {
    native_origin: u64,
    deadline: Option<Instant>,
    holder_owned: bool,
    closes: Vec<serde_json::Value>,
    pending_alias: Option<(String, OwnedFd)>,
    complete: bool,
}
fn decode_argument(text: &str) -> io::Result<OsString> {
    require(
        text.len().is_multiple_of(2)
            && text.len() <= 131_072
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "keeper argv hex differs",
    )?;
    let mut bytes = Vec::with_capacity(text.len() / 2);
    for p in text.as_bytes().as_chunks::<2>().0 {
        let t = std::str::from_utf8(p).unwrap();
        bytes.push(u8::from_str_radix(t, 16).map_err(io::Error::other)?);
    }
    require(!bytes.contains(&0), "keeper argv contains NUL")?;
    Ok(OsString::from_vec(bytes))
}
impl Keeper {
    pub fn retain(parent: OwnedFd, deadline: Instant, native_deadline: u64) -> Self {
        Self {
            parent: wire::Channel::retain(parent),
            parent_credentials: None,
            parent_pidfd: None,
            holder: None,
            source_endpoint: None,
            guardian_endpoint: None,
            wrapper_pidfd: None,
            configuration: None,
            deadline,
            native_deadline,
            stage: Stage::Retained,
            refused: None,
            closed_guardian: false,
            failed_report_attempted: false,
            startup_cancellation: None,
            startup_report_attempted: false,
            configuration_retirement: None,
            configuration_failure_origin: None,
            passcred_attempts: [None, None],
            rejected_startup: None,
            uncaptured_startup: None,
            successful_exit_attempted: false,
            successful_exit_sent: false,
            terminal_export: None,
            creation_cleanup: None,
            source_bridge: None,
            mirrored_prefix: None,
            initial_prefix_attempted: false,
        }
    }
    fn check(&self) -> io::Result<()> {
        if let Some(e) = &self.refused {
            return Err(e.error());
        }
        require(
            Instant::now() < self.deadline,
            "keeper original stage deadline expired",
        )?;
        if let Some(fd) = &self.parent_pidfd {
            require(
                !owner::terminal(fd.as_raw_fd())?,
                "keeper parent is terminal; retained recovery required",
            )?;
        }
        Ok(())
    }
    /// Separately driven successful terminal transfer. The ordinary progress
    /// loop still owns all source/query protocol transitions. No failed Keeper
    /// path enters this method or repairs its original journal.
    pub fn progress_success_export(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            require(
                self.stage == Stage::Source
                    && self.rejected_startup.is_none()
                    && self.startup_cancellation.is_none()
                    && self.configuration_retirement.is_none(),
                "successful export outside original source phase",
            )?;
            let holder = self
                .holder
                .as_mut()
                .ok_or_else(|| io::Error::other("successful export lacks original Holder"))?;
            if !self.successful_exit_sent {
                if !holder.source_exit_observed() {
                    return Ok(false);
                }
                require(
                    !self.successful_exit_attempted,
                    "successful exit notice cannot repeat",
                )?;
                self.successful_exit_attempted = true;
                let record = holder.prepare_success_exit_notice()?;
                super::send_after_durability(&mut self.parent, self.deadline, &record)?;
                require(
                    Instant::now() < self.deadline
                        && super::guardian::monotonic_ns()? < self.native_deadline,
                    "successful exit notice completed after original stage",
                )?;
                self.successful_exit_sent = true;
                return Ok(true);
            }
            if !holder.is_completed_stage() {
                return Ok(false);
            }
            if self.terminal_export.is_none() {
                self.terminal_export = Some(super::adoption::TerminalExport::retain());
                self.terminal_export.as_mut().unwrap().prepare(holder)?;
            }
            let export = self.terminal_export.as_mut().unwrap();
            if export.acknowledged() {
                return Ok(false);
            }
            if !export.sent() {
                return export.send_next(holder, &mut self.parent);
            }
            export.receive_ack(holder, &mut self.parent, self.parent_credentials.unwrap())
        })();
        self.remember(result)
    }
    /// Permission for this private actor entry to return naturally. This is not
    /// parent-side terminal proof: every original alias remains owned until the
    /// actual process exit, and the parent must observe strict EOF, both pipe
    /// EOFs, pidfd, natural wait, logs/fsync, group absence and full ECHILD.
    pub fn retire_local_custody(
        &mut self,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<bool> {
        self.refused.get_or_insert_with(|| Failure::capture(cause));
        if let Some(holder) = &mut self.holder {
            return holder.retire_local_custody(deadline, cause);
        }
        // No Holder means no query was spawned; the process still retains all
        // partial input rights until its actual natural failure exit.
        owner::check_no_children()?;
        Ok(true)
    }
    pub fn ready_for_success_exit(&self) -> bool {
        self.refused.is_none()
            && self.successful_exit_sent
            && self
                .terminal_export
                .as_ref()
                .is_some_and(|state| state.acknowledged())
    }
    fn remember<T>(&mut self, r: io::Result<T>) -> io::Result<T> {
        if let Err(e) = &r {
            if self.refused.is_none()
                && self.stage == Stage::Configuration
                && self.configuration.is_some()
                && self.parent.sends.is_empty()
            {
                self.configuration_failure_origin = guardian::monotonic_ns().ok();
            }
            self.refused.get_or_insert_with(|| Failure::capture(e));
        }
        r
    }
    pub fn initialize(&mut self) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(self.stage == Stage::Retained, "keeper initialized twice")?;
            owner::protected_holder()?;
            self.parent.validate()?;
            require(
                self.deadline.saturating_duration_since(Instant::now()) <= Duration::from_secs(20),
                "keeper original20s bound extended",
            )?;
            let mut peer = std::mem::MaybeUninit::<libc::ucred>::uninit();
            let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    self.parent.fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    peer.as_mut_ptr().cast(),
                    &mut size,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            require(
                size as usize == std::mem::size_of::<libc::ucred>(),
                "keeper parent credentials truncated",
            )?;
            let peer = unsafe { peer.assume_init() };
            require(
                peer.pid > 0
                    && peer.pid != unsafe { libc::getpid() }
                    && peer.uid == unsafe { libc::getuid() }
                    && peer.gid == unsafe { libc::getgid() },
                "keeper parent endpoint identity differs",
            )?;
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, peer.pid, 0) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            self.parent_pidfd = Some(unsafe { OwnedFd::from_raw_fd(raw as i32) });
            owner::pidfd_matches(self.parent_pidfd.as_ref().unwrap().as_raw_fd(), peer.pid)?;
            self.parent_credentials = Some(wire::Credentials {
                pid: peer.pid,
                uid: peer.uid,
                gid: peer.gid,
            });
            self.stage = Stage::Configuration;
            Ok(())
        })();
        self.remember(result)
    }
    pub fn progress(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            match self.stage {
                Stage::Configuration => {
                    let Some(index) = self.parent.receive(65_536)? else {
                        return Ok(false);
                    };
                    let packet = &mut self.parent.packets[index];
                    packet.exact(3, self.parent_credentials.unwrap())?;
                    let config: Configuration = serde_json::from_slice(&packet.bytes)?;
                    require(
                        journal::canonical(&serde_json::to_value(&config)?)? == packet.bytes
                            && config.schema == "hermit-grouped-keeper-config-v1",
                        "keeper configuration grammar differs",
                    )?;
                    let native_now = guardian::monotonic_ns()?;
                    require(
                        config.stage_deadline == self.native_deadline
                            && config.stage_deadline > native_now
                            && config.stage_deadline - native_now <= 20_000_000_000,
                        "keeper source deadline differs",
                    )?;
                    require(
                        !config.arguments.is_empty() && config.arguments.len() <= 128,
                        "keeper argument count exceeds fixed population",
                    )?;
                    let arguments = config
                        .arguments
                        .iter()
                        .map(|s| decode_argument(s))
                        .collect::<io::Result<Vec<_>>>()?;
                    let intent = Intent::new(config.nonce.clone(), config.incarnation)?;
                    self.configuration = Some(config);
                    let config = self.configuration.as_ref().unwrap();
                    // The received originals stay in retained Packet custody
                    // while socketpair can still fail.
                    let mut pair = [-1; 2];
                    if unsafe {
                        libc::socketpair(
                            libc::AF_UNIX,
                            libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                            0,
                            pair.as_mut_ptr(),
                        )
                    } != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    let source = unsafe { OwnedFd::from_raw_fd(pair[0]) };
                    self.source_endpoint = Some(unsafe { OwnedFd::from_raw_fd(pair[1]) });
                    let image = packet.rights.remove(0);
                    self.guardian_endpoint = Some(packet.rights.remove(0));
                    let directory = packet.rights.remove(0);
                    self.holder = Some(guardian::Holder::retain(
                        intent,
                        config.unit.clone(),
                        (self.deadline, config.stage_deadline),
                        guardian::Role::Keeper,
                        (source, directory),
                        image,
                        arguments,
                    ));
                    // PASSCRED before either endpoint can cross to the source.
                    for (index, fd) in [pair[0], pair[1]].into_iter().enumerate() {
                        let yes = 1i32;
                        require(
                            self.passcred_attempts[index].is_none(),
                            "configuration PASSCRED call cannot retry",
                        )?;
                        self.passcred_attempts[index] = Some(PasscredAttempt { fd, raw: None });
                        let raw = unsafe {
                            libc::setsockopt(
                                fd,
                                libc::SOL_SOCKET,
                                libc::SO_PASSCRED,
                                (&yes as *const i32).cast(),
                                4,
                            )
                        };
                        let error = (raw != 0).then(io::Error::last_os_error);
                        self.passcred_attempts[index].as_mut().unwrap().raw = Some(wire::RawCall {
                            returned: raw as isize,
                            errno: error.as_ref().and_then(io::Error::raw_os_error),
                        });
                        if let Some(error) = error {
                            return Err(error);
                        }
                    }
                    if self.creation_cleanup.is_some() {
                        self.holder.as_mut().unwrap().install_creation_cleanup()?;
                    }
                    if self.source_bridge.is_some() {
                        self.holder
                            .as_mut()
                            .unwrap()
                            .install_external_creation_mirror()?;
                    }
                    self.holder.as_mut().unwrap().initialize()?;
                    if let Some(bridge) = &self.source_bridge {
                        self.holder
                            .as_mut()
                            .unwrap()
                            .forward_source_bridge(&bridge.configuration, bridge.file.as_fd())?;
                    }
                    let packet = journal::canonical(
                        &json!({"schema":"hermit-grouped-source-channel-v1","nonce":config.nonce,"incarnation":config.incarnation,"stage_deadline":config.stage_deadline}),
                    )?;
                    self.stage = Stage::Launched;
                    self.parent
                        .send_once(&packet, &[self.source_endpoint.as_ref().unwrap().as_fd()])?;
                    Ok(true)
                }
                Stage::Launched => {
                    let Some(index) = self.parent.receive(2048)? else {
                        return Ok(false);
                    };
                    let packet = &mut self.parent.packets[index];
                    let grammar: serde_json::Value = serde_json::from_slice(&packet.bytes)?;
                    if grammar["schema"] == "hermit-cancel-before-source-v1" {
                        packet.exact(0, self.parent_credentials.unwrap())?;
                        let cancel: CancelBeforeSource = serde_json::from_slice(&packet.bytes)?;
                        let config = self.configuration.as_ref().unwrap();
                        require(
                            journal::canonical(&serde_json::to_value(&cancel)?)? == packet.bytes
                                && cancel.nonce == config.nonce
                                && cancel.incarnation == config.incarnation
                                && cancel.stage_deadline == config.stage_deadline,
                            "startup cancellation identity differs",
                        )?;
                        require(
                            self.startup_cancellation.is_none() && self.wrapper_pidfd.is_none(),
                            "startup cancellation after launcher ownership",
                        )?;
                        self.startup_cancellation = Some(cancel);
                        return Err(io::Error::other("keeper source launch cancelled"));
                    }
                    packet.exact(1, self.parent_credentials.unwrap())?;
                    let launched: Launched = serde_json::from_slice(&packet.bytes)?;
                    let config = self.configuration.as_ref().unwrap();
                    require(
                        journal::canonical(&serde_json::to_value(&launched)?)? == packet.bytes
                            && launched.schema == "hermit-grouped-source-launched-v1"
                            && launched.nonce == config.nonce
                            && launched.incarnation == config.incarnation
                            && launched.stage_deadline == config.stage_deadline,
                        "keeper launcher handoff differs",
                    )?;
                    self.wrapper_pidfd = Some(packet.rights.remove(0));
                    owner::pidfd_matches(
                        self.wrapper_pidfd.as_ref().unwrap().as_raw_fd(),
                        launched.wrapper,
                    )?;
                    // Explicitly close only this keeper's sent source alias. The
                    // parent retains the launcher and the source retains stdin.
                    close_alias(self.source_endpoint.take().unwrap())?;
                    self.stage = Stage::Source;
                    Ok(true)
                }
                Stage::Source => {
                    let changed = self.holder.as_mut().unwrap().progress()?;
                    if self.holder.as_ref().unwrap().creator_acknowledged() && !self.closed_guardian
                    {
                        self.holder
                            .as_mut()
                            .unwrap()
                            .forward_guardian(self.guardian_endpoint.as_ref().unwrap().as_fd())?;
                        self.closed_guardian = true;
                        close_alias(self.guardian_endpoint.take().unwrap())?;
                    }
                    Ok(changed)
                }
                Stage::Retained => Err(io::Error::other("keeper not initialized")),
            }
        })();
        self.remember(result)
    }
}

// This legacy failure protocol is not reached by the maintained source entry.
// Keep its checked transitions intact; compiling them is not qualification.
#[expect(dead_code, reason = "Legacy keeper failure entrypoints are not integrated")]
impl Keeper {
    /// Before a source endpoint was ever sent, retire only original local
    /// configuration custody. This never repairs an uninitialized journal.
    /// Retain the original configuration-failure origin without extending it.
    pub fn begin_configuration_retirement(&mut self, native_origin: u64) -> io::Result<()> {
        if let Some(previous) = &self.configuration_retirement {
            return require(
                previous.native_origin == native_origin,
                "configuration retirement cannot reset original origin",
            );
        }
        require(
            self.configuration_failure_origin == Some(native_origin),
            "configuration retirement differs from original first-failure origin",
        )?;
        require(
            self.stage == Stage::Configuration
                && self.refused.is_some()
                && self.configuration.is_some()
                && self.wrapper_pidfd.is_none()
                && self.parent.sends.is_empty()
                && self.startup_cancellation.is_none(),
            "configuration retirement already attempted source handoff or lacks valid config",
        )?;
        if let Some(holder) = &mut self.holder {
            let passcred_failure = self.passcred_attempts[0]
                .as_ref()
                .and_then(|a| a.raw)
                .filter(|r| r.returned == -1);
            holder.retain_unforwarded_configuration_failure(
                &self.refused.as_ref().unwrap().error(),
                passcred_failure,
            )?;
            holder.check_unforwarded_configuration()?;
        }
        require(
            self.holder.is_some() == self.source_endpoint.is_some()
                && self.holder.is_some() == self.guardian_endpoint.is_some(),
            "configuration custody topology differs",
        )?;
        self.configuration_retirement = Some(ConfigurationRetirement {
            native_origin,
            deadline: None,
            holder_owned: self.holder.is_some(),
            closes: Vec::new(),
            pending_alias: None,
            complete: false,
        });
        let sampled = Instant::now();
        let now = guardian::monotonic_ns()?;
        require(
            native_origin <= now && now - native_origin < 1_000_000_000,
            "configuration retirement original1s expired or future",
        )?;
        self.configuration_retirement.as_mut().unwrap().deadline = Some(
            (sampled + Duration::from_nanos(1_000_000_000 - (now - native_origin)))
                .min(self.deadline),
        );
        self.configuration_retirement_deadline().map(|_| ())
    }
    fn configuration_retirement_deadline(&self) -> io::Result<Instant> {
        let state = self
            .configuration_retirement
            .as_ref()
            .ok_or_else(|| io::Error::other("configuration retirement was not begun"))?;
        let now = guardian::monotonic_ns()?;
        require(
            now >= state.native_origin && now - state.native_origin < 1_000_000_000,
            "configuration retirement original1s expired or future",
        )?;
        let deadline = state
            .deadline
            .ok_or_else(|| io::Error::other("configuration retirement origin was refused"))?;
        require(
            Instant::now() < deadline,
            "configuration retirement original deadline expired",
        )?;
        Ok(deadline)
    }
    fn close_configuration_alias(&mut self, name: &str, alias: OwnedFd) -> io::Result<()> {
        use std::os::fd::IntoRawFd;
        // Move the original alias into retained state before any fallible check.
        let state = self.configuration_retirement.as_mut().unwrap();
        require(
            state.pending_alias.is_none(),
            "configuration close already retains an unresolved alias",
        )?;
        state.pending_alias = Some((name.to_owned(), alias));
        self.configuration_retirement_deadline()?;
        let fd = self
            .configuration_retirement
            .as_ref()
            .unwrap()
            .pending_alias
            .as_ref()
            .unwrap()
            .1
            .as_raw_fd();
        let identity = owner::stat(fd)?;
        let record = json!({"role":name,"fd":fd,"device":identity.device,"inode":identity.inode,"mode":identity.mode,
            "returned":null,"errno":null});
        let rows = &mut self.configuration_retirement.as_mut().unwrap().closes;
        require(
            rows.len() < 3,
            "configuration close population exceeds three owned aliases",
        )?;
        rows.push(record);
        let index = rows.len() - 1;
        let (_, alias) = self
            .configuration_retirement
            .as_mut()
            .unwrap()
            .pending_alias
            .take()
            .unwrap();
        let raw = unsafe { libc::close(alias.into_raw_fd()) };
        let error = (raw < 0).then(io::Error::last_os_error);
        let rows = &mut self.configuration_retirement.as_mut().unwrap().closes;
        rows[index]["returned"] = json!(raw);
        rows[index]["errno"] = json!(error.as_ref().and_then(io::Error::raw_os_error));
        if let Some(error) = error {
            return Err(error);
        }
        require(raw == 0, "configuration alias close result differs")?;
        self.configuration_retirement_deadline().map(|_| ())
    }
    pub fn progress_configuration_retirement(&mut self) -> io::Result<bool> {
        let deadline = self.configuration_retirement_deadline()?;
        require(
            self.stage == Stage::Configuration
                && self.refused.is_some()
                && self.wrapper_pidfd.is_none()
                && self.parent.sends.is_empty(),
            "configuration retirement acquired source authority",
        )?;
        let state = self.configuration_retirement.as_ref().unwrap();
        require(
            state.pending_alias.is_none(),
            "configuration close retains an unresolved original alias",
        )?;
        require(
            state
                .closes
                .iter()
                .all(|v| v["returned"] == 0 && v["errno"].is_null()),
            "configuration alias close remains unknown or failed",
        )?;
        if state.complete {
            return Ok(true);
        }
        if state.holder_owned {
            if let Some(alias) = self.source_endpoint.take() {
                self.close_configuration_alias("unforwarded-source-peer", alias)?;
            }
            if !self
                .holder
                .as_mut()
                .unwrap()
                .observe_unforwarded_configuration_eof(deadline)?
            {
                return Ok(false);
            }
            if let Some(alias) = self.guardian_endpoint.take() {
                self.close_configuration_alias("unforwarded-guardian", alias)?;
                self.closed_guardian = true;
            }
        } else {
            require(
                self.source_endpoint.is_none() && self.guardian_endpoint.is_none(),
                "configuration acquired unrecorded socket pair",
            )?;
            // The valid atomic config is still in its original received packet.
            require(
                self.parent.packets.len() == 1,
                "configuration packet inventory differs",
            )?;
            while !self.parent.packets[0].rights.is_empty() {
                let remaining = self.parent.packets[0].rights.len();
                let alias = self.parent.packets[0].rights.remove(0);
                let role = match remaining {
                    3 => "configuration-image",
                    2 => "configuration-guardian",
                    1 => "configuration-journal-directory",
                    _ => return Err(io::Error::other("configuration rights population differs")),
                };
                self.close_configuration_alias(role, alias)?;
                if remaining == 2 {
                    self.closed_guardian = true;
                }
            }
        }
        self.configuration_retirement_deadline()?;
        self.configuration_retirement.as_mut().unwrap().complete = true;
        Ok(true)
    }
    pub fn configuration_retirement_observation(&self) -> io::Result<serde_json::Value> {
        self.configuration_retirement_deadline()?;
        let state = self.configuration_retirement.as_ref().unwrap();
        require(
            state.complete
                && self.closed_guardian
                && self.source_endpoint.is_none()
                && self.guardian_endpoint.is_none(),
            "configuration custody retirement incomplete",
        )?;
        Ok(
            json!({"schema":"hermit-unforwarded-configuration-custody-v1","native_origin":state.native_origin,
            "nonce":self.configuration.as_ref().unwrap().nonce,"original_failure":self.refused.as_ref().unwrap().message,
            "original_errno":self.refused.as_ref().unwrap().errno,"actual_passcred_calls":self.passcred_attempts.each_ref().map(|a|a.as_ref().map(PasscredAttempt::observation)),"source_handoff_attempted":!self.parent.sends.is_empty(),
            "source_wrapper_received":self.wrapper_pidfd.is_some(),"holder_owned":state.holder_owned,"alias_closes":state.closes,
            "holder":self.holder.as_ref().map(guardian::Holder::unforwarded_configuration_observation).transpose()?,
            "history_complete_claimed":false,"provider_authority_created":false}),
        )
    }
    pub fn progress_cancelled_startup_retirement(&mut self) -> io::Result<bool> {
        require(
            self.stage == Stage::Launched
                && self.wrapper_pidfd.is_none()
                && self
                    .refused
                    .as_ref()
                    .is_some_and(|e| e.message == "keeper source launch cancelled"),
            "cancelled startup has launcher ownership or wrong failure",
        )?;
        let origin = self
            .startup_cancellation
            .as_ref()
            .ok_or_else(|| io::Error::other("startup cancellation was not received"))?
            .native_origin;
        let holder = self
            .holder
            .as_mut()
            .ok_or_else(|| io::Error::other("cancelled startup lacks retained holder"))?;
        holder.begin_unstarted_retirement(origin)?;
        if let Some(alias) = self.source_endpoint.take() {
            close_alias(alias)?;
        }
        if !holder.progress_unstarted_retirement()? {
            return Ok(false);
        }
        if let Some(alias) = self.guardian_endpoint.take() {
            close_alias(alias)?;
            self.closed_guardian = true;
        }
        Ok(true)
    }
    pub fn cancelled_startup_observation(&mut self) -> io::Result<serde_json::Value> {
        require(
            self.source_endpoint.is_none()
                && self.guardian_endpoint.is_none()
                && self.wrapper_pidfd.is_none(),
            "cancelled startup retains source or guardian aliases",
        )?;
        let holder = self
            .holder
            .as_mut()
            .ok_or_else(|| io::Error::other("cancelled startup holder absent"))?;
        let observation = holder.unstarted_retirement_observation()?;
        Ok(
            json!({"schema":"hermit-keeper-unstarted-custody-v1","keeper_failure":self.refused.as_ref().unwrap().message,
            "source_wrapper_received":false,"source_alias_closed":true,"guardian_alias_closed":self.closed_guardian,
            "observation":observation}),
        )
    }
    pub fn send_cancelled_startup_observation(&mut self) -> io::Result<()> {
        require(
            !self.startup_report_attempted,
            "cancelled startup report cannot be retried",
        )?;
        let observation = self.cancelled_startup_observation()?;
        let bytes = journal::canonical(&observation)?;
        require(
            bytes.len() <= 4096,
            "cancelled startup report exceeds original packet bound",
        )?;
        self.startup_report_attempted = true;
        self.parent.send_once(&bytes, &[])
    }
    pub fn receive_cancelled_startup_agreement(&mut self) -> io::Result<bool> {
        let own = self.cancelled_startup_observation()?;
        let Some(index) = self.parent.receive(4096)? else {
            return Ok(false);
        };
        let packet = &self.parent.packets[index];
        packet.exact(0, self.parent_credentials.unwrap())?;
        require(
            packet.bytes
                == journal::canonical(
                    &json!({"schema":"hermit-unstarted-custody-agreement-v1","keeper":own}),
                )?,
            "unstarted custody agreement differs",
        )?;
        self.holder
            .as_mut()
            .unwrap()
            .record_unstarted_agreement(&own)?;
        Ok(true)
    }
    pub fn receive_failed_retirement_request(&mut self) -> io::Result<bool> {
        #[derive(Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        struct Request {
            schema: String,
            nonce: String,
            native_origin: u64,
        }
        require(
            self.refused.is_some(),
            "keeper retirement request precedes original failure",
        )?;
        let Some(index) = self.parent.receive(4096)? else {
            return Ok(false);
        };
        let packet = &self.parent.packets[index];
        packet.exact(0, self.parent_credentials.unwrap())?;
        let request: Request = serde_json::from_slice(&packet.bytes)?;
        require(
            journal::canonical(&serde_json::to_value(&request)?)? == packet.bytes
                && request.schema == "hermit-failed-source-retirement-request-v1"
                && self
                    .configuration
                    .as_ref()
                    .is_some_and(|c| c.nonce == request.nonce),
            "keeper failed retirement request differs",
        )?;
        self.begin_failed_retirement(request.native_origin)?;
        Ok(true)
    }
    pub fn send_failed_retirement_observation(&mut self) -> io::Result<()> {
        require(
            !self.failed_report_attempted,
            "keeper failed custody report cannot retry",
        )?;
        let observation = self.failed_retirement_observation()?;
        let packet = journal::canonical(&observation)?;
        require(
            packet.len() <= 4096,
            "keeper failed custody report exceeds original packet bound",
        )?;
        self.failed_report_attempted = true;
        self.parent.send_once(&packet, &[])
    }
    pub fn receive_failed_retirement_agreement(&mut self) -> io::Result<bool> {
        let own = self.failed_retirement_observation()?;
        let Some(index) = self.parent.receive(4096)? else {
            return Ok(false);
        };
        let packet = &self.parent.packets[index];
        packet.exact(0, self.parent_credentials.unwrap())?;
        let value: serde_json::Value = serde_json::from_slice(&packet.bytes)?;
        require(
            journal::canonical(&value)? == packet.bytes
                && value.as_object().is_some_and(|o| o.len() == 3)
                && value["schema"] == "hermit-failed-source-history-agreement-v1"
                && value["keeper"] == own,
            "failed holder history agreement differs",
        )?;
        guardian::check_failed_observations(&value["guardian"], &own)?;
        self.holder
            .as_mut()
            .unwrap()
            .record_failed_agreement(&value)?;
        Ok(true)
    }
    pub fn begin_failed_retirement(&mut self, native_origin: u64) -> io::Result<()> {
        require(
            self.refused.is_some(),
            "keeper cleanup requires original refusal",
        )?;
        self.holder
            .as_mut()
            .ok_or_else(|| io::Error::other("keeper holder custody absent"))?
            .begin_failed_retirement(native_origin)
    }
    /// Keeper rejected its actual captured source before any ACK/Guardian
    /// forwarding. Its original Guardian alias remains owned until source EOF.
    pub fn begin_rejected_startup_retirement(&mut self, caller: Instant) -> io::Result<()> {
        require(
            self.refused.is_some()
                && self.stage == Stage::Source
                && self.wrapper_pidfd.is_some()
                && self.source_endpoint.is_none()
                && self.guardian_endpoint.is_some()
                && !self.closed_guardian,
            "keeper rejection lacks original launched unforwarded topology",
        )?;
        if self.rejected_startup.is_none() {
            self.rejected_startup = Some(RejectedStartup {
                phase: RejectedPhase::Queries,
                failure: None,
                query_observation: None,
                holder_observation: None,
                guardian_close: None,
                report_attempted: false,
                agreement_attempted: false,
                agreement_acknowledged: false,
            });
        }
        let result = (|| {
            let holder = self
                .holder
                .as_mut()
                .ok_or_else(|| io::Error::other("keeper rejected Holder absent"))?;
            holder.keeper_unadmitted_observation()?;
            holder.begin_inflight_retirement(caller.min(self.deadline))
        })();
        if let Err(error) = &result {
            self.rejected_startup
                .as_mut()
                .unwrap()
                .failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    pub fn progress_rejected_startup_retirement(&mut self) -> io::Result<bool> {
        let result = (|| {
            let state = self
                .rejected_startup
                .as_ref()
                .ok_or_else(|| io::Error::other("keeper rejected cleanup was not begun"))?;
            if let Some(error) = &state.failure {
                return Err(error.error());
            }
            require(
                self.refused.is_some() && self.stage == Stage::Source,
                "keeper rejected cleanup lost original refusal",
            )?;
            if state.phase == RejectedPhase::Complete {
                self.holder
                    .as_ref()
                    .unwrap()
                    .keeper_rejected_cleanup_deadline()?;
                return Ok(true);
            }
            let holder = self.holder.as_mut().unwrap();
            if state.phase == RejectedPhase::Queries {
                if !holder.progress_inflight_queries()? {
                    return Ok(false);
                }
                self.rejected_startup.as_mut().unwrap().query_observation =
                    Some(holder.inflight_observation()?);
                self.rejected_startup.as_mut().unwrap().phase = RejectedPhase::Revoking;
                holder.begin_keeper_rejected_creator_retirement()?;
                holder.shutdown_keeper_rejected_creator_writer()?;
                self.rejected_startup.as_mut().unwrap().phase = RejectedPhase::Source;
            }
            require(
                self.rejected_startup.as_ref().unwrap().phase == RejectedPhase::Source,
                "keeper rejected source revocation is incomplete",
            )?;
            if !holder.progress_failed_retirement()? {
                return Ok(false);
            }
            if !owner::terminal(self.wrapper_pidfd.as_ref().unwrap().as_raw_fd())? {
                return Ok(false);
            }
            let observation = holder.failed_retirement_observation()?;
            require(
                observation["creator_admitted"] == false
                    && observation["controls_held"].is_null()
                    && observation["history"]
                        == json!({"next":1,"pending":null,"pairs":[],"failed_pair":null,"create_mask":0}),
                "keeper first rejection gained admission, controls or nonempty history",
            )?;
            self.rejected_startup.as_mut().unwrap().holder_observation = Some(observation);
            require(
                self.rejected_startup
                    .as_ref()
                    .unwrap()
                    .guardian_close
                    .is_none()
                    && !self.closed_guardian,
                "keeper rejected Guardian alias close cannot repeat",
            )?;
            let fd = self
                .guardian_endpoint
                .as_ref()
                .ok_or_else(|| io::Error::other("keeper rejected original Guardian alias absent"))?
                .as_raw_fd();
            let identity = owner::stat(fd)?;
            self.rejected_startup.as_mut().unwrap().guardian_close =
                Some(json!({"state":"intent-only","fd":fd,
                "device":identity.device,"inode":identity.inode,"mode":identity.mode}));
            use std::os::fd::IntoRawFd;
            let original = self.guardian_endpoint.take().unwrap().into_raw_fd();
            self.rejected_startup
                .as_mut()
                .unwrap()
                .guardian_close
                .as_mut()
                .unwrap()["state"] = json!("submitted-result-unknown");
            let raw = unsafe { libc::close(original) };
            let error = (raw < 0).then(io::Error::last_os_error);
            let close = self
                .rejected_startup
                .as_mut()
                .unwrap()
                .guardian_close
                .as_mut()
                .unwrap();
            close["state"] = json!("returned");
            close["returned"] = json!(raw);
            close["errno"] = json!(error.as_ref().and_then(io::Error::raw_os_error));
            if let Some(error) = error {
                return Err(error);
            }
            require(
                raw == 0,
                "keeper rejected alias close native result differs",
            )?;
            self.closed_guardian = true;
            // The Holder validates its original cutoff again after the effect.
            holder.failed_retirement_observation()?;
            self.rejected_startup.as_mut().unwrap().phase = RejectedPhase::Complete;
            Ok(true)
        })();
        if let Err(error) = &result
            && let Some(state) = &mut self.rejected_startup
        {
                state.failure.get_or_insert_with(|| Failure::capture(error));
            }
        result
    }
    pub fn send_rejected_startup_observation(&mut self) -> io::Result<()> {
        let state = self
            .rejected_startup
            .as_mut()
            .ok_or_else(|| io::Error::other("keeper rejected cleanup absent"))?;
        require(
            state.phase == RejectedPhase::Complete
                && state.failure.is_none()
                && !state.report_attempted,
            "keeper rejected report is incomplete or repeated",
        )?;
        let current = self
            .holder
            .as_mut()
            .unwrap()
            .failed_retirement_observation()?;
        require(
            state.holder_observation.as_ref() == Some(&current),
            "keeper rejected source observation changed",
        )?;
        let packet = journal::canonical(&current)?;
        require(
            packet.len() <= 4096,
            "keeper rejected report exceeds original packet bound",
        )?;
        state.report_attempted = true;
        self.parent.send_once(&packet, &[])
    }
    pub fn receive_rejected_startup_agreement(&mut self) -> io::Result<bool> {
        let result = (|| {
            let state = self
                .rejected_startup
                .as_ref()
                .ok_or_else(|| io::Error::other("keeper rejected cleanup absent"))?;
            if let Some(error) = &state.failure {
                return Err(error.error());
            }
            require(
                state.phase == RejectedPhase::Complete
                    && state.report_attempted
                    && !state.agreement_attempted,
                "keeper early agreement precedes report or repeats",
            )?;
            self.holder
                .as_mut()
                .unwrap()
                .failed_retirement_observation()?;
            let Some(index) = self.parent.receive(4096)? else {
                return Ok(false);
            };
            self.rejected_startup.as_mut().unwrap().agreement_attempted = true;
            let packet = &self.parent.packets[index];
            packet.exact(0, self.parent_credentials.unwrap())?;
            let value: serde_json::Value = serde_json::from_slice(&packet.bytes)?;
            require(
                journal::canonical(&value)? == packet.bytes,
                "keeper early agreement packet is not canonical",
            )?;
            let ack = self
                .holder
                .as_mut()
                .unwrap()
                .record_keeper_first_agreement(&value)?;
            self.parent.send_once(&ack, &[])?;
            self.holder
                .as_mut()
                .unwrap()
                .failed_retirement_observation()?;
            self.rejected_startup
                .as_mut()
                .unwrap()
                .agreement_acknowledged = true;
            Ok(true)
        })();
        if let Err(error) = &result
            && let Some(state) = &mut self.rejected_startup
        {
                state.failure.get_or_insert_with(|| Failure::capture(error));
            }
        result
    }
    pub fn rejected_startup_observation(&self) -> io::Result<serde_json::Value> {
        let state = self
            .rejected_startup
            .as_ref()
            .ok_or_else(|| io::Error::other("keeper rejected cleanup absent"))?;
        Ok(
            json!({"phase":format!("{:?}",state.phase),"cleanup_failure":state.failure.as_ref().map(|e|&e.message),
            "queries":state.query_observation,"holder":state.holder_observation,"guardian_close":state.guardian_close,
            "actual_unadmitted":self.holder.as_ref().unwrap().keeper_unadmitted_observation()?,"report_attempted":state.report_attempted,
            "agreement_attempted":state.agreement_attempted,"agreement_acknowledged":state.agreement_acknowledged,
            "original_failure":self.refused.as_ref().map(|e|&e.message),"provider_authority_created":false}),
        )
    }
    /// Cancellation before either initial query is parsed. The original
    /// source/Guardian endpoints and wrapper identity remain owned here.
    pub fn cancel_uncaptured_startup(&mut self, cause: &io::Error) -> io::Result<()> {
        self.check()?;
        require(
            self.stage == Stage::Source
                && self.wrapper_pidfd.is_some()
                && self.source_endpoint.is_none()
                && self.guardian_endpoint.is_some()
                && !self.closed_guardian
                && self.rejected_startup.is_none()
                && self.uncaptured_startup.is_none(),
            "uncaptured cancellation lacks original launched topology",
        )?;
        let result = self
            .holder
            .as_mut()
            .ok_or_else(|| io::Error::other("uncaptured Holder absent"))?
            .cancel_uncaptured_queries(cause);
        self.remember(result)?;
        self.refused = Some(Failure::capture(cause));
        Ok(())
    }
    pub fn begin_uncaptured_startup_retirement(&mut self, caller: Instant) -> io::Result<()> {
        require(
            self.refused.is_some()
                && self.stage == Stage::Source
                && self.wrapper_pidfd.is_some()
                && self.source_endpoint.is_none()
                && self.guardian_endpoint.is_some()
                && !self.closed_guardian
                && self.rejected_startup.is_none()
                && self.uncaptured_startup.is_none(),
            "uncaptured cleanup lacks original cancellation topology or repeats",
        )?;
        self.uncaptured_startup = Some(RejectedStartup {
            phase: RejectedPhase::Queries,
            failure: None,
            query_observation: None,
            holder_observation: None,
            guardian_close: None,
            report_attempted: false,
            agreement_attempted: false,
            agreement_acknowledged: false,
        });
        let result = self
            .holder
            .as_mut()
            .unwrap()
            .begin_partial_creator_retirement(caller.min(self.deadline));
        if let Err(error) = &result {
            self.uncaptured_startup
                .as_mut()
                .unwrap()
                .failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    pub fn progress_uncaptured_startup_retirement(&mut self) -> io::Result<bool> {
        let result = (|| {
            let state = self
                .uncaptured_startup
                .as_ref()
                .ok_or_else(|| io::Error::other("uncaptured cleanup not begun"))?;
            if let Some(error) = &state.failure {
                return Err(error.error());
            }
            require(
                self.refused.is_some() && self.stage == Stage::Source,
                "uncaptured cleanup lost original failure",
            )?;
            let holder = self.holder.as_mut().unwrap();
            holder.inflight_cleanup_deadline()?;
            if state.phase == RejectedPhase::Complete {
                return Ok(true);
            }
            if state.phase == RejectedPhase::Queries {
                if !holder.progress_inflight_queries()? {
                    return Ok(false);
                }
                self.uncaptured_startup.as_mut().unwrap().query_observation =
                    Some(holder.inflight_observation()?);
                self.uncaptured_startup.as_mut().unwrap().phase = RejectedPhase::Revoking;
                holder.shutdown_inflight_writer()?;
                self.uncaptured_startup.as_mut().unwrap().phase = RejectedPhase::Source;
            }
            require(
                self.uncaptured_startup.as_ref().unwrap().phase == RejectedPhase::Source,
                "uncaptured revocation outcome is incomplete",
            )?;
            if !holder.progress_partial_creator_retirement()? {
                return Ok(false);
            }
            if !owner::terminal(self.wrapper_pidfd.as_ref().unwrap().as_raw_fd())? {
                return Ok(false);
            }
            let observation = holder.partial_creator_retirement_observation()?;
            require(
                observation["creator_captured"] == false
                    && observation["creator_admitted"] == false
                    && observation["controls_held"].is_null()
                    && observation["manager_snapshot_constructed"] == false,
                "uncaptured cleanup acquired admission, controls or Snapshot",
            )?;
            self.uncaptured_startup.as_mut().unwrap().holder_observation = Some(observation);
            require(
                self.uncaptured_startup
                    .as_ref()
                    .unwrap()
                    .guardian_close
                    .is_none()
                    && !self.closed_guardian,
                "uncaptured Guardian alias close cannot repeat",
            )?;
            let fd = self
                .guardian_endpoint
                .as_ref()
                .ok_or_else(|| io::Error::other("uncaptured original Guardian alias absent"))?
                .as_raw_fd();
            let identity = owner::stat(fd)?;
            self.uncaptured_startup.as_mut().unwrap().guardian_close =
                Some(json!({"state":"intent-only","fd":fd,
                "device":identity.device,"inode":identity.inode,"mode":identity.mode}));
            holder.inflight_cleanup_deadline()?;
            use std::os::fd::IntoRawFd;
            let original = self.guardian_endpoint.take().unwrap().into_raw_fd();
            self.uncaptured_startup
                .as_mut()
                .unwrap()
                .guardian_close
                .as_mut()
                .unwrap()["state"] = json!("submitted-result-unknown");
            let raw = unsafe { libc::close(original) };
            let error = (raw < 0).then(io::Error::last_os_error);
            let close = self
                .uncaptured_startup
                .as_mut()
                .unwrap()
                .guardian_close
                .as_mut()
                .unwrap();
            close["state"] = json!("returned");
            close["returned"] = json!(raw);
            close["errno"] = json!(error.as_ref().and_then(io::Error::raw_os_error));
            if let Some(error) = error {
                return Err(error);
            }
            require(raw == 0, "uncaptured Guardian close native result differs")?;
            self.closed_guardian = true;
            holder.inflight_cleanup_deadline()?;
            self.uncaptured_startup.as_mut().unwrap().phase = RejectedPhase::Complete;
            Ok(true)
        })();
        if let Err(error) = &result
            && let Some(state) = &mut self.uncaptured_startup
        {
                state.failure.get_or_insert_with(|| Failure::capture(error));
            }
        result
    }
    pub fn send_uncaptured_startup_observation(&mut self) -> io::Result<()> {
        let state = self
            .uncaptured_startup
            .as_mut()
            .ok_or_else(|| io::Error::other("uncaptured cleanup absent"))?;
        require(
            state.phase == RejectedPhase::Complete
                && state.failure.is_none()
                && !state.report_attempted,
            "uncaptured report incomplete or repeated",
        )?;
        let current = self
            .holder
            .as_mut()
            .unwrap()
            .partial_creator_retirement_observation()?;
        require(
            state.holder_observation.as_ref() == Some(&current),
            "uncaptured source observation changed",
        )?;
        let bytes = journal::canonical(&current)?;
        require(
            bytes.len() <= 4096,
            "uncaptured report exceeds original packet bound",
        )?;
        state.report_attempted = true;
        self.parent.send_once(&bytes, &[])?;
        self.holder
            .as_ref()
            .unwrap()
            .inflight_cleanup_deadline()
            .map(|_| ())
    }
    pub fn receive_uncaptured_startup_agreement(&mut self) -> io::Result<bool> {
        let result = (|| {
            let state = self
                .uncaptured_startup
                .as_ref()
                .ok_or_else(|| io::Error::other("uncaptured cleanup absent"))?;
            if let Some(error) = &state.failure {
                return Err(error.error());
            }
            require(
                state.phase == RejectedPhase::Complete
                    && state.report_attempted
                    && !state.agreement_attempted,
                "uncaptured agreement precedes report or repeats",
            )?;
            self.holder.as_ref().unwrap().inflight_cleanup_deadline()?;
            let Some(index) = self.parent.receive(4096)? else {
                return Ok(false);
            };
            self.uncaptured_startup
                .as_mut()
                .unwrap()
                .agreement_attempted = true;
            let packet = &self.parent.packets[index];
            packet.exact(0, self.parent_credentials.unwrap())?;
            let value: serde_json::Value = serde_json::from_slice(&packet.bytes)?;
            require(
                journal::canonical(&value)? == packet.bytes,
                "uncaptured agreement is not canonical",
            )?;
            let ack = self
                .holder
                .as_mut()
                .unwrap()
                .record_partial_keeper_agreement(&value)?;
            self.parent.send_once(&ack, &[])?;
            self.holder.as_ref().unwrap().inflight_cleanup_deadline()?;
            self.uncaptured_startup
                .as_mut()
                .unwrap()
                .agreement_acknowledged = true;
            Ok(true)
        })();
        if let Err(error) = &result
            && let Some(state) = &mut self.uncaptured_startup
        {
                state.failure.get_or_insert_with(|| Failure::capture(error));
            }
        result
    }
    pub fn uncaptured_startup_observation(&self) -> io::Result<serde_json::Value> {
        let state = self
            .uncaptured_startup
            .as_ref()
            .ok_or_else(|| io::Error::other("uncaptured cleanup absent"))?;
        Ok(
            json!({"phase":format!("{:?}",state.phase),"cleanup_failure":state.failure.as_ref().map(|e|&e.message),
            "queries":state.query_observation,"holder":state.holder_observation,"guardian_close":state.guardian_close,
            "actual_uncaptured":self.holder.as_ref().unwrap().keeper_uncaptured_observation()?,
            "report_attempted":state.report_attempted,"agreement_attempted":state.agreement_attempted,
            "agreement_acknowledged":state.agreement_acknowledged,"original_failure":self.refused.as_ref().map(|e|&e.message),
            "provider_authority_created":false}),
        )
    }
    pub fn progress_failed_retirement(&mut self) -> io::Result<bool> {
        require(
            self.refused.is_some(),
            "keeper cleanup lost original refusal",
        )?;
        let holder = self
            .holder
            .as_mut()
            .ok_or_else(|| io::Error::other("keeper holder custody absent"))?;
        if !holder.progress_failed_retirement()? {
            return Ok(false);
        }
        let wrapper = self
            .wrapper_pidfd
            .as_ref()
            .ok_or_else(|| io::Error::other("keeper original wrapper pidfd absent"))?;
        require(
            owner::terminal(wrapper.as_raw_fd())?,
            "failed source wrapper remains live",
        )?;
        Ok(true)
    }
    pub fn failed_retirement_observation(&mut self) -> io::Result<serde_json::Value> {
        require(
            self.refused.is_some(),
            "keeper cleanup lost original refusal",
        )?;
        self.holder
            .as_mut()
            .ok_or_else(|| io::Error::other("keeper holder custody absent"))?
            .failed_retirement_observation()
    }
    pub fn controls_ready(&self) -> bool {
        self.holder
            .as_ref()
            .is_some_and(guardian::Holder::controls_ready)
            && self.refused.is_none()
    }
    pub fn diagnostics(&self) -> io::Result<serde_json::Value> {
        Ok(
            json!({"stage":format!("{:?}",self.stage),"holder":self.holder.as_ref().map(guardian::Holder::diagnostics).transpose()?,"parent_pidfd_held":self.parent_pidfd.is_some(),"wrapper_pidfd_held":self.wrapper_pidfd.is_some(),"guardian_alias_closed":self.closed_guardian,"failure":self.refused.as_ref().map(|e|&e.message)}),
        )
    }
}
fn close_alias(fd: OwnedFd) -> io::Result<()> {
    use std::os::fd::IntoRawFd;
    let raw = fd.into_raw_fd();
    if unsafe { libc::close(raw) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Keeper {
    #[expect(dead_code, reason = "Legacy creation-cleanup installer is not called by the maintained entry")]
    pub(super) fn install_creation_cleanup(
        &mut self,
        peer: super::cleanup::CreationPeer,
    ) -> io::Result<()> {
        require(
            self.stage == Stage::Retained && self.creation_cleanup.is_none(),
            "Keeper cleanup must precede configuration",
        )?;
        self.creation_cleanup = Some(peer);
        self.creation_cleanup.as_mut().unwrap().initialize()
    }
    pub(super) fn progress_creation_cleanup(&mut self) -> io::Result<bool> {
        require(
            self.creation_cleanup.is_some(),
            "Keeper creation cleanup absent",
        )?;
        if self.stage != Stage::Source {
            return self.progress();
        }
        let peer = self.creation_cleanup.as_mut().unwrap();
        let changed = peer.progress(
            self.holder.as_mut().unwrap(),
            &mut self.parent,
            self.parent_credentials
                .ok_or_else(|| io::Error::other("cleanup original parent credentials absent"))?,
            self.parent_pidfd
                .as_ref()
                .ok_or_else(|| io::Error::other("cleanup original parent pidfd absent"))?
                .as_fd(),
        )?;
        if peer.source_protocol_active() {
            // Retain a real source-protocol failure while the original parent
            // cancellation packet is in flight. No successful progress or new
            // source ACK is adopted after the Holder's original refusal.
            if let Err(error) = self.progress() {
                self.refused.get_or_insert_with(|| Failure::capture(&error));
            }
        }
        Ok(changed)
    }
    #[expect(dead_code, reason = "Legacy creation-cleanup completion query is not integrated")]
    pub(super) fn creation_cleanup_complete(&self) -> bool {
        self.creation_cleanup
            .as_ref()
            .is_some_and(|peer| peer.complete())
    }
}
impl Keeper {
    /// The full-success source entry retains its prepared failed-prefix owner,
    /// but after actual natural full17 completion the parent channel belongs
    /// to the unchanged terminal export. A terminal-export ACK must never be
    /// consumed by CreationPeer as though it were a cancellation request.
    fn progress_external_prefix(&mut self) -> io::Result<bool> {
        let sequence = self
            .holder
            .as_ref()
            .and_then(guardian::Holder::held_creation_sequence)
            .ok_or_else(|| io::Error::other("Keeper mirror has no held actual callback"))?;
        let holder = self.holder.as_mut().unwrap();
        let (intent, _, _, native, _) = holder.original_context();
        let intent = intent.clone();
        if self.mirrored_prefix.is_none() {
            let history = holder.creation_source_history()?;
            require(
                history.frames.len() as u64 == sequence && (1..=34).contains(&sequence),
                "Keeper mirror differs from actual durable callback prefix",
            )?;
            let value = json!({"schema":"hermit-grouped-creation-keeper-prefix-v1","nonce":intent.nonce,
                "incarnation":intent.incarnation,"stage_deadline":native,"sequence":sequence,
                "keeper_store":history.commitment()});
            self.mirrored_prefix = Some((sequence, value.clone()));
            self.parent.send_once(&journal::canonical(&value)?, &[])?;
        }
        let (expected_sequence, expected) = self.mirrored_prefix.as_ref().unwrap();
        require(
            *expected_sequence == sequence,
            "Keeper held callback changed during mirror",
        )?;
        let Some(index) = self.parent.receive(4096)? else {
            return Ok(false);
        };
        let packet = &self.parent.packets[index];
        packet.exact(0, self.parent_credentials.unwrap())?;
        let mut response = expected.clone();
        response["schema"] = json!("hermit-grouped-creation-keeper-prefix-ack-v1");
        require(
            packet.bytes == journal::canonical(&response)?,
            "Keeper mirror release differs from exact retained prefix",
        )?;
        require(
            !owner::terminal(self.parent_pidfd.as_ref().unwrap().as_raw_fd())?,
            "original controller terminal before Keeper mirror release",
        )?;
        let history = holder.creation_source_history()?;
        require(
            history.commitment() == expected["keeper_store"],
            "Keeper Store changed before actual callback release",
        )?;
        holder.acknowledge_mirrored_keeper_callback(sequence)?;
        self.mirrored_prefix = None;
        Ok(true)
    }
    pub(super) fn progress_successful_creation(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            require(
                self.creation_cleanup.is_some() && self.source_bridge.is_some(),
                "successful creation entry lacks original preparation",
            )?;
            if !self.initial_prefix_attempted
                && self
                    .holder
                    .as_ref()
                    .is_some_and(guardian::Holder::controls_ready)
            {
                let holder = self.holder.as_mut().unwrap();
                let history = holder.creation_source_history()?;
                require(
                    history.frames.is_empty(),
                    "initial Keeper Store already contains callbacks",
                )?;
                let (intent, _, _, native, _) = holder.original_context();
                let value = json!({"schema":"hermit-grouped-creation-keeper-initial-v1","nonce":intent.nonce,
                    "incarnation":intent.incarnation,"stage_deadline":native,"keeper_store":history.commitment()});
                self.initial_prefix_attempted = true;
                let namespace = holder.source_namespace()?;
                self.parent
                    .send_once(&journal::canonical(&value)?, &[namespace.fd()])?;
                return Ok(true);
            }
            if self
                .holder
                .as_ref()
                .and_then(guardian::Holder::held_creation_sequence)
                .is_some()
            {
                return self.progress_external_prefix();
            }
            if self
                .holder
                .as_ref()
                .is_some_and(guardian::Holder::source_exit_observed)
            {
                self.holder.as_mut().unwrap().successful_exit_record()?;
                let changed = self.progress()?;
                return self
                    .progress_success_export()
                    .map(|exported| changed || exported);
            }
            self.progress_creation_cleanup()
        })();
        self.remember(result)
    }
    /// The closed entry calls this before the unchanged three-right source
    /// configuration. Every original descriptor stays installed on refusal.
    pub(super) fn install_source_bridge(&mut self, intent: &Intent) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.stage == Stage::Configuration
                    && self.configuration.is_none()
                    && self.source_bridge.is_none(),
                "source bridge installation is late or repeated",
            )?;
            let index = loop {
                self.check()?;
                if let Some(index) = self.parent.receive(4096)? {
                    break index;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            let packet = &mut self.parent.packets[index];
            packet.exact(1, self.parent_credentials.unwrap())?;
            let configuration: super::entry::BridgeConfiguration =
                serde_json::from_slice(&packet.bytes)?;
            require(
                journal::canonical(&serde_json::to_value(&configuration)?)? == packet.bytes,
                "source bridge install is not canonical",
            )?;
            configuration.check(intent, &configuration.unit, self.native_deadline)?;
            self.source_bridge = Some(SourceBridge {
                configuration,
                file: packet.rights.remove(0),
            });
            let file = self.source_bridge.as_ref().unwrap().file.as_raw_fd();
            require(
                unsafe { libc::fcntl(file, libc::F_GET_SEALS) }
                    == libc::F_SEAL_WRITE
                        | libc::F_SEAL_GROW
                        | libc::F_SEAL_SHRINK
                        | libc::F_SEAL_SEAL,
                "source bridge has no exact immutable seals",
            )?;
            self.check()
        })();
        self.remember(result)
    }
    pub(super) fn receive_creation_install(&mut self, intent: Intent) -> io::Result<()> {
        require(
            self.stage == Stage::Configuration && self.creation_cleanup.is_none(),
            "creation cleanup installation is late",
        )?;
        let index = loop {
            self.check()?;
            if let Some(index) = self.parent.receive(4096)? {
                break index;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let packet = &mut self.parent.packets[index];
        packet.exact(1, self.parent_credentials.unwrap())?;
        require(
            packet.bytes
                == journal::canonical(
                    &json!({"schema":"hermit-cleanup-install-v1","nonce":intent.nonce,
            "incarnation":intent.incarnation,"stage_deadline":self.native_deadline}),
                )?,
            "creation cleanup installation packet differs",
        )?;
        self.creation_cleanup = Some(super::cleanup::CreationPeer::retain(
            packet.rights.remove(0),
            intent,
            self.deadline,
            self.native_deadline,
        ));
        self.creation_cleanup.as_mut().unwrap().initialize()
    }
}
