use super::*;

/// # Safety
/// Invoked only by the early private CLI path, before other threads/guests.
/// Its stdin is the original authenticated launch endpoint and roles are the
/// exact three manager-opened source controls. The Keeper delivers the sealed
/// bridge from the same maintained package that this launch authenticated.
pub unsafe fn run_grouped_source_process(
    input: OwnedFd,
    controls: [OwnedFd; 3],
    unit: String,
    run: [u8; 16],
    incarnation: u64,
    deadline: u64,
) -> ! {
    let mut custody = match EntryCustody::retain(input, unit, run, incarnation, deadline) {
        Ok(custody) => custody,
        Err(error) => finish_entry(Err(error)),
    };
    let mut bridge: Option<native::Bridge> = None;
    let mut namespace: Option<OwnedFd> = None;
    // Diagnostic samples carry their own errors and never become deadlines.
    let sample = || {
        let saved_errno = unsafe { *libc::__errno_location() };
        let observation = match guardian::monotonic_ns() {
            Ok(now) => (Some(now), None, false),
            Err(error) => (None, error.raw_os_error(), true),
        };
        unsafe {
            *libc::__errno_location() = saved_errno;
        }
        observation
    };
    let mut admission_times = [(None, None, false); 6];
    let mut native_call: Option<(bool, Option<i32>)> = None;
    admission_times[0] = sample();
    let result: io::Result<()> = (|| {
        custody.initialize()?;
        admission_times[1] = sample();
        let index = custody.receive(4096)?;
        let packet = &mut custody.channel.packets[index];
        packet.exact(1, custody.peer.unwrap())?;
        let configuration: BridgeConfiguration = serde_json::from_slice(&packet.bytes)?;
        require(
            journal::canonical(&serde_json::to_value(&configuration)?)? == packet.bytes,
            "source bridge configuration is not canonical",
        )?;
        let digest = configuration.check(&custody.intent, &custody.unit, deadline)?;
        // Install real native custody before loading or validating the artifact.
        bridge = Some(native::Bridge::retain(packet.rights.remove(0), digest));
        unsafe {
            bridge.as_mut().unwrap().initialize()?;
        }
        admission_times[2] = sample();
        // Capture our actual manager-created namespace before original EXEC.
        // The original helper1s is already running and is never restarted.
        namespace = Some(std::fs::File::open("/proc/self/ns/mnt")?.into());
        custody.announce_creator_with_namespace(Some(namespace.as_ref().unwrap().as_fd()))?;
        admission_times[3] = sample();
        let nonce = CString::new(custody.intent.nonce.clone()).map_err(io::Error::other)?;
        let unit = CString::new(custody.unit.clone()).map_err(io::Error::other)?;
        admission_times[4] = sample();
        let native_result = bridge.as_mut().unwrap().source(
            custody.channel.fd.as_fd(),
            incarnation,
            &nonce,
            deadline,
            custody.creator_cutoff,
            &unit,
            controls.each_ref().map(AsFd::as_fd),
        );
        native_call = Some((
            native_result.is_ok(),
            native_result
                .as_ref()
                .err()
                .and_then(io::Error::raw_os_error),
        ));
        admission_times[5] = sample();
        native_result?;
        custody.check(true)?;
        let ready = bridge.as_mut().unwrap().status()?;
        require(
            ready.source_ready == 1
                && ready.source_created == 0
                && ready.refused == 0
                && ready.attempted_sites == 0
                && ready.verified_sites == 0,
            "actual source bridge is not the original unwritten owner",
        )?;
        // Actual maintained17-role create. Each effect is gated by both live
        // owners' original durable callbacks; there is no synthetic history.
        bridge.as_mut().unwrap().create()?;
        custody.check(false)?;
        let created = bridge.as_mut().unwrap().status()?;
        require(
            created.source_created == 1
                && created.refused == 0
                && created.attempted_sites == 0x1ffff
                && created.verified_sites == 0x1ffff,
            "actual source did not complete all seventeen native pairs",
        )?;
        Ok(())
    })();
    if let Err(error) = &result {
        guardian::emit_startup_failure_diagnostic(&json!({
            "schema":"hermit-grouped-source-startup-timing-at-failure-v1","nonce":custody.intent.nonce,
            "creator_cutoff":custody.creator_cutoff,"stage_deadline":deadline,
            "primary":error.to_string(),"errno":error.raw_os_error(),
            "marks":["retained","initialized","bridge_initialized","keeper_exec_received","native_call_enter","native_call_return"],
            "samples_ns_errno_failed":admission_times,"native_call_ok_errno":native_call,
            "creator_sent":custody.creator_sent,"creator_accepted":custody.creator_accepted}));
    }
    // Even failed native startup retains the context here until one explicit
    // local release attempt; it never asks this creator to delete definitions.
    let cleanup = if let Some(bridge) = &mut bridge {
        bridge.release_aliases().and_then(|()| bridge.finish())
    } else {
        Ok(())
    };
    if let Err(error) = &cleanup {
        eprintln!("grouped source local release refused: {error}");
    }
    let result = result.and(cleanup);
    drop(namespace);
    drop(controls);
    drop(custody);
    finish_entry(result)
}

/// # Safety
/// Same closed early process and original manager identity contract as source.
/// The receiver authenticates this actual Creator before EXEC, retains all
/// leaves before ACK and joins the real wrapper before treating it terminal.
pub unsafe fn run_grouped_leaf_delegate_process(
    input: OwnedFd,
    leaves: [OwnedFd; 3],
    unit: String,
    run: [u8; 16],
    incarnation: u64,
    deadline: u64,
) -> ! {
    let mut custody = match EntryCustody::retain(input, unit, run, incarnation, deadline) {
        Ok(custody) => custody,
        Err(error) => finish_entry(Err(error)),
    };
    let result = (|| {
        custody.initialize()?;
        custody.announce_creator()?;
        let mut value = json!({"schema":"hermit-grouped-leaves-v1", "nonce":custody.intent.nonce,
            "incarnation":incarnation, "stage_deadline":deadline, "unit":custody.unit,
            "roles":["ID","FORMAT","ENABLE"]});
        custody.channel.send_once(
            &journal::canonical(&value)?,
            &leaves.each_ref().map(AsFd::as_fd),
        )?;
        let index = custody.receive(4096)?;
        let packet = &custody.channel.packets[index];
        packet.exact(0, custody.peer.unwrap())?;
        value["schema"] = json!("hermit-grouped-leaves-ack-v1");
        require(
            packet.bytes == journal::canonical(&value)?,
            "leaf delegate did not receive exact original custody ACK",
        )?;
        custody.check(true)
    })();
    drop(leaves);
    drop(custody);
    finish_entry(result)
}
