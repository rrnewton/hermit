//! Original controller-local custody receipt. A transported report is accepted
//! only from the held controller channel, after that native controller is dead.
//! It never constructs a Launcher or substitutes for the local source owner.
use super::*;

pub(super) struct ControllerCustody {
    header: Value,
    file: OwnedFd,
    bytes: Vec<u8>,
    raw: Option<wire::RawCall>,
    report: Option<Value>,
    verified: bool,
}
impl ControllerCustody {
    fn verify(&mut self, intent: &Intent, stage: u64, origin: u64, cutoff: u64) -> io::Result<()> {
        require(
            !self.verified && self.raw.is_none(),
            "controller custody report read cannot repeat",
        )?;
        common::before(cutoff)?;
        let identity = owner::stat(self.file.as_raw_fd())?;
        let flags = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_GETFL) };
        let count = self.header["report_bytes"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| io::Error::other("controller custody report size absent"))?;
        require(
            count > 0
                && count <= 1_048_576
                && identity.size == count as i64
                && identity.mode & libc::S_IFMT == libc::S_IFREG
                && identity.mode & 0o7777 == 0o600
                && identity.uid == unsafe { libc::getuid() }
                && identity.links == 1
                && flags >= 0
                && flags & (libc::O_ACCMODE | libc::O_PATH) == libc::O_RDONLY
                && unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_GETFD) } == libc::FD_CLOEXEC,
            "controller custody is not its original bounded readonly private report",
        )?;
        self.bytes.resize(count + 1, 0);
        let raw = unsafe {
            libc::pread(
                self.file.as_raw_fd(),
                self.bytes.as_mut_ptr().cast(),
                self.bytes.len(),
                0,
            )
        };
        let error = (raw < 0).then(io::Error::last_os_error);
        self.raw = Some(wire::RawCall {
            returned: raw,
            errno: error.as_ref().and_then(io::Error::raw_os_error),
        });
        if let Some(error) = error {
            self.bytes.clear();
            return Err(error);
        }
        self.bytes.truncate(raw as usize);
        let after = owner::stat(self.file.as_raw_fd())?;
        require(
            after.same_owner(&identity)
                && after.links == identity.links
                && after.size == identity.size
                && self.bytes.len() == count
                && self.header["report_sha256"] == hex(&Sha256::digest(&self.bytes)),
            "controller actual report bytes or original file changed",
        )?;
        self.report = Some(serde_json::from_slice(&self.bytes)?);
        let report = self.report.as_ref().unwrap();
        require(
            journal::canonical(report)? == self.bytes,
            "controller custody report is not canonical",
        )?;
        let records = report["records"]
            .as_array()
            .ok_or_else(|| io::Error::other("controller actual custody records absent"))?;
        for row in records {
            let kind = row["kind"]
                .as_str()
                .ok_or_else(|| io::Error::other("controller custody kind absent"))?;
            let role = row["role"]
                .as_str()
                .filter(|r| !r.is_empty())
                .ok_or_else(|| io::Error::other("controller custody role absent"))?;
            let actual = &row["record"];
            require(
                (kind == "launcher" || kind == "query")
                    && actual.is_object()
                    && actual.get("pid").is_some()
                    && row == &json!({"kind":kind,"role":role,"record":actual}),
                "controller custody row shape differs",
            )?;
            if let Some(pid) = actual["pid"].as_i64() {
                require(
                    pid > 0
                        && actual["eof"] == json!([true, true])
                        && actual["raw_wait_status"].is_number()
                        && actual["retirement_failure"].is_null(),
                    "controller actual child has no terminal EOF/wait custody",
                )?;
                if kind == "launcher" {
                    require(
                        actual["custody_retired"] == true
                            || (actual["raw_wait_status"] == 0 && actual["logs_synced"] == true),
                        "controller original Launcher custody did not complete",
                    )?;
                } else {
                    require(
                        actual["custody_retired"] == true
                            || (actual["initialized"] == true && actual["raw_wait_status"] == 0),
                        "controller original query custody did not complete",
                    )?;
                }
            } else {
                require(
                    kind == "query"
                        && actual["pid"].is_null()
                        && actual["raw_wait_status"].is_null(),
                    "controller absent query fabricated a child wait",
                )?;
            }
        }
        let launchers = records
            .iter()
            .filter(|r| r["kind"] == "launcher" && r["record"]["pid"].is_number())
            .count();
        let queries = records
            .iter()
            .filter(|r| r["kind"] == "query" && r["record"]["pid"].is_number())
            .count();
        let waitid = json!({"idtype":"P_ALL","options":["WEXITED","WNOHANG","WNOWAIT"],"returned":-1,"errno":libc::ECHILD});
        require(
            report
                == &json!({"schema":"hermit-grouped-controller-local-custody-report-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":stage,"first_failure_origin":origin,
            "records":records,"launcher_count":launchers,"query_count":queries,"waitid":waitid}),
            "controller actual report identity/counts/global wait differ",
        )?;
        require(
            self.header
                == json!({"schema":"hermit-grouped-runtime-controller-custody-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":stage,"first_failure_origin":origin,
            "report_bytes":count,"report_sha256":hex(&Sha256::digest(&self.bytes)),
            "launcher_count":launchers,"query_count":queries,"waitid":waitid}),
            "controller custody packet differs from its actual retained report",
        )?;
        common::before(cutoff)?;
        self.verified = true;
        Ok(())
    }
}

impl RuntimeKeeper {
    pub(super) fn retain_controller_failure(&mut self, value: Value) -> io::Result<io::Error> {
        require(
            self.controller_refusal.is_none(),
            "original controller failure notice cannot repeat",
        )?;
        self.controller_refusal = Some(value.clone());
        let origin = value["first_failure_origin"]
            .as_u64()
            .ok_or_else(|| io::Error::other("early source failure has no original origin"))?;
        let cause = value["cause"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| io::Error::other("early source first cause absent"))?;
        let intent = self.intent()?;
        require(
            value
                == json!({"schema":"hermit-grouped-runtime-creation-refused-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":self.stage,
            "first_failure_origin":origin,"cause":cause})
                && origin != 0
                && origin <= guardian::monotonic_ns()?,
            "early source changed first failure identity/origin",
        )?;
        let error = io::Error::other(cause.to_owned());
        if self.failure.is_none() {
            self.failure = Some(Failure::capture(&error));
            self.failure_origin = Some(origin);
        } else {
            self.peer_failure_origin = Some(
                self.peer_failure_origin
                    .map_or(origin, |old| old.min(origin)),
            );
        }
        Ok(error)
    }
    pub(super) fn poll_controller_custody(&mut self, cutoff: u64) -> io::Result<()> {
        if self.controller_eof {
            return common::before(cutoff);
        }
        common::before(cutoff)?;
        let link = self.startup.as_mut().unwrap();
        let Some(index) = link.channel.receive(4096)? else {
            return Ok(());
        };
        let packet = &mut link.channel.packets[index];
        if packet.raw.returned == 0 {
            require(
                packet.raw.errno.is_none()
                    && packet.bytes.is_empty()
                    && packet.credentials.is_empty()
                    && packet.rights.is_empty()
                    && packet.rights_messages == 0
                    && packet.flags == libc::MSG_CMSG_CLOEXEC,
                "original controller EOF contains unread bytes or ancillary custody",
            )?;
            self.controller_eof = true;
            return common::before(cutoff);
        }
        let value: Value = serde_json::from_slice(&packet.bytes)?;
        require(
            packet.bytes == journal::canonical(&value)?,
            "controller custody packet noncanonical",
        )?;
        match value["schema"].as_str() {
            Some("hermit-grouped-runtime-creation-refused-v1") => {
                packet.exact(0, link.credentials)?;
                self.retain_controller_failure(value)?;
            }
            Some("hermit-grouped-runtime-controller-custody-v1") => {
                packet.exact(1, link.credentials)?;
                require(
                    self.controller_custody.is_none() && self.controller_refusal.is_some(),
                    "original controller custody report precedes failure or repeats",
                )?;
                self.controller_custody = Some(custody::ControllerCustody {
                    header: value,
                    file: packet.rights.remove(0),
                    bytes: Vec::new(),
                    raw: None,
                    report: None,
                    verified: false,
                });
            }
            _ => {
                return Err(io::Error::other(
                    "unexpected controller packet after creation refusal/handoff",
                ));
            }
        }
        common::before(cutoff)
    }
    pub(super) fn finish_controller_custody(&mut self, cutoff: u64) -> io::Result<()> {
        require(
            owner::terminal(self.startup.as_ref().unwrap().pidfd.as_raw_fd())?,
            "controller-local custody report precedes actual original terminality",
        )?;
        while !self.controller_eof {
            self.poll_controller_custody(cutoff)?;
            if !self.controller_eof {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        if let Some(refusal) = &self.controller_refusal {
            let origin = refusal["first_failure_origin"].as_u64().unwrap();
            let intent = self.intent()?.clone();
            self.controller_custody
                .as_mut()
                .ok_or_else(|| {
                    io::Error::other("ordinary failed controller lacks actual local custody report")
                })?
                .verify(&intent, self.stage, origin, cutoff)?;
            let bytes = self.controller_custody.as_ref().unwrap().bytes.clone();
            self.write_file("controller-original-local-custody.json", &bytes, cutoff)?;
            self.early_callbacks.as_mut().unwrap().ledger.as_mut().unwrap().store.append(json!({
                "kind":"actual-original-controller-local-custody","bytes":bytes.len(),"sha256":hex(&Sha256::digest(&bytes))}))?;
        } else {
            require(
                self.parent_join_reply.as_ref().is_some_and(|v| {
                    v["schema"] == "hermit-grouped-runtime-startup-already-joined-v1"
                }),
                "unreported controller terminality cannot prove its original local child/query population",
            )?;
        }
        common::before(cutoff)
    }
    pub(super) fn controller_custody_descriptor(&self) -> Option<i32> {
        self.controller_custody.as_ref().map(|r| r.file.as_raw_fd())
    }
}
