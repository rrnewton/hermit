use super::*;

pub(super) fn validate(
    context: &ResourceRecoveryContext,
    intent: &Value,
    intent_raw: &[u8],
    result: &Value,
) -> io::Result<()> {
    let i = array(intent, 11)?;
    let r = array(result, 12)?;
    require(
        text(&i[0])? == "hermit-failed-resource-intent-v1"
            && text(&r[0])? == "hermit-failed-resource-result-v1",
        "resource protocol differs",
    )?;
    let boot = text(&i[1])?;
    require(
        uuid::Uuid::parse_str(boot).is_ok()
            && boot == std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim()
            && r[2] == i[1],
        "resource boot differs",
    )?;
    require(
        hex(&i[5], 64)? == digest(PRODUCER)
            && r[11] == i[5]
            && hex(&r[1], 64)? == digest(intent_raw),
        "resource producer or intent hash differs",
    )?;
    let started = number(&i[2])?;
    let action_deadline = number(&i[3])?;
    require(
        started > 0
            && started.checked_add(5_000_000_000) == Some(action_deadline)
            && number(&i[4])? == u64::from(unsafe { libc::getuid() }),
        "resource action interval or owner differs",
    )?;
    let a = array(&i[6], 7)?;
    let u = array(&i[7], 8)?;
    for (path, identity, root) in [
        (&a[0], &a[1], &context.accepted),
        (&u[0], &u[1], &context.unix),
        (&u[5], &u[6], &context.pins),
    ] {
        require(
            Path::new(text(path)?) == root.path && identity == &root.identity,
            "resource configured root differs",
        )?;
    }
    let pin_directory = array(&u[7], 4)?;
    require(
        number(&pin_directory[0])? == number(&u[6][0])?
            && number(&pin_directory[1])? > 0
            && number(&pin_directory[2])? == u64::from(libc::S_IFDIR | 0o700)
            && pin_directory[3] == i[4],
        "resource original pin directory differs",
    )?;
    let pin_name = format!("ugb1-{}", &label(&u[2])?[..16]);
    match context.pins.path.join(&pin_name).symlink_metadata() {
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
        _ => {
            return Err(io::Error::other(
                "resource owned pin directory remains or absence is unknown",
            ));
        }
    }
    require(
        number(&a[6])? > 0 && number(&a[6])? <= started,
        "resource original close chronology differs",
    )?;
    let ids = object_ids(&i[9])?;
    require(
        hex(&i[10], 64)? == digest(&serde_json::to_vec(&i[9])?),
        "resource inventory hash differs",
    )?;
    let closed = number(&r[4])?;
    let deadline = number(&r[5])?;
    let final_ns = number(&r[9])?;
    require(
        started <= closed
            && closed <= action_deadline
            && closed > 0
            && closed.checked_add(1_000_000_000) == Some(deadline)
            && closed <= final_ns
            && final_ns <= deadline,
        "resource scanner interval differs",
    )?;
    let journal = array(&u[4], 6)?;
    let pins = list(&journal[4])?;
    require(pins.len() == 41, "resource original pin population differs")?;
    let actions = array(&r[3], 42)?;
    let mut previous = started;
    let mut inode_set = BTreeSet::new();
    for (index, action) in actions.iter().enumerate() {
        let action = array(action, 10)?;
        let at = number(&action[7])?;
        require(
            previous <= at
                && at <= closed
                && number(&action[8])? == 0
                && number(&action[9])? == u64::from(libc::ENOENT as u32),
            "resource action result or order differs",
        )?;
        previous = at;
        if index < 41 {
            let pin = array(&pins[index], 3)?;
            let expected_name = if index < 31 {
                format!("l{index:02}")
            } else {
                format!("m{:02}", index - 31)
            };
            require(
                text(&pin[0])? == expected_name
                    && action[0] == pin[0]
                    && number(&pin[1])? == u64::from(index < 31)
                    && number(&action[1])? == if index < 31 { 2 } else { 0 }
                    && action[2] == pin[2]
                    && number(&action[2])? > 0
                    && action[3] == pin_directory[0]
                    && number(&action[4])? > 0
                    && number(&action[5])? == u64::from(libc::S_IFREG | 0o600)
                    && action[6] == i[4],
                "resource pin action differs from original population",
            )?;
            require(
                inode_set.insert(number(&action[4])?),
                "resource pin inodes alias",
            )?;
        } else {
            require(
                text(&action[0])? == "@directory"
                    && number(&action[1])? == 3
                    && number(&action[2])? == 0
                    && action[3..7] == pin_directory[..],
                "resource directory removal differs",
            )?;
        }
    }
    let scanner = array(&r[6], 6)?;
    require(
        number(&scanner[0])? > 0
            && number(&scanner[0])? <= i32::MAX as u64
            && number(&scanner[1])? > 0
            && number(&scanner[2])? == 0
            && number(&scanner[3])? == 0
            && scanner[5] == i[5]
            && number(&r[7])? == 0,
        "resource scanner identity or wait differs",
    )?;
    hex(&scanner[4], 64)?;
    let mut previous = closed;
    for scan in array(&r[8], 2)? {
        let scan = array(scan, 3)?;
        let begin = number(&scan[0])?;
        let end = number(&scan[1])?;
        require(
            previous <= begin && begin <= end && end <= final_ns,
            "resource scan chronology differs",
        )?;
        previous = end;
        let answers = array(&scan[2], ids.len())?;
        for (answer, id) in answers.iter().zip(&ids) {
            let answer = array(answer, 4)?;
            require(
                answer[..3] == id[..] && number(&answer[3])? == libc::ENOENT as u64,
                "resource scan omitted, repeated or did not prove an original ID absent",
            )?;
        }
    }
    require(r[10] == i[8], "resource actors changed after scanning")?;
    actors(&i[8], label(&a[3])?, label(&u[2])?, started)?;
    Ok(())
}

pub(super) fn object_ids(value: &Value) -> io::Result<Vec<[Value; 3]>> {
    let rows = array(value, 194)?;
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    let mut counts = [[0usize; 3]; 2];
    let mut previous = None;
    for row in rows {
        let row = array(row, 3)?;
        let (domain, kind, id) = (number(&row[0])?, number(&row[1])?, number(&row[2])?);
        require(
            domain < 2 && kind < 3 && id > 0 && id <= u32::MAX as u64,
            "resource object identifier differs",
        )?;
        let key = (domain, kind, id);
        require(
            previous.is_none_or(|p| p < key) && seen.insert(key),
            "resource inventory is not unique and ordered",
        )?;
        previous = Some(key);
        counts[domain as usize][kind as usize] += 1;
        result.push([row[0].clone(), row[1].clone(), row[2].clone()]);
    }
    require(
        counts == [[24, 49, 49], [10, 31, 31]],
        "resource inventory topology differs",
    )?;
    Ok(result)
}
fn actors(value: &Value, run: &str, unix_label: &str, started: u64) -> io::Result<()> {
    let units = [
        format!("hermit-accepted-{run}.service"),
        format!("hermit-accepted-readback-{run}.service"),
        format!("hermit-unix-{unix_label}.service"),
    ];
    const KEYS: &[&str] = &[
        "ActiveState",
        "ControlGroup",
        "ExecMainCode",
        "ExecMainExitTimestampMonotonic",
        "ExecMainPID",
        "ExecMainStartTimestampMonotonic",
        "ExecMainStatus",
        "ExecStart",
        "FragmentPath",
        "Id",
        "InvocationID",
        "LoadState",
        "MainPID",
        "SubState",
    ];
    for (index, actor) in array(value, 3)?.iter().enumerate() {
        let a = array(actor, 4)?;
        require(text(&a[0])? == units[index], "resource actor unit differs")?;
        let mut properties = std::collections::BTreeMap::new();
        let mut previous = "";
        for pair in list(&a[1])? {
            let pair = array(pair, 2)?;
            let key = text(&pair[0])?;
            let value = text(&pair[1])?;
            require(
                KEYS.contains(&key) && previous < key && properties.insert(key, value).is_none(),
                "resource manager keys differ",
            )?;
            previous = key;
        }
        require(
            properties.get("Id") == Some(&units[index].as_str()),
            "resource manager identity differs",
        )?;
        let get = |key| {
            properties
                .get(key)
                .copied()
                .ok_or_else(|| io::Error::other("resource manager field absent"))
        };
        if index < 2 {
            let original = array(&a[2], 4)?;
            label(&original[0])?;
            require(
                text(&original[1])? == format!("/sys/fs/cgroup/system.slice/{}", units[index])
                    && number(&original[2])? > 0
                    && number(&original[3])? > 0
                    && a[3].is_null(),
                "resource original accepted actor differs",
            )?;
        } else {
            require(a[2].is_null(), "resource fabricated historical Unix actor")?;
        }
        if index < 2 && get("LoadState")? == "not-found" {
            continue;
        }
        require(
            get("LoadState")? == "loaded"
                && get("ActiveState")? == "failed"
                && get("SubState")? == "failed"
                && get("MainPID")? == "0"
                && get("ControlGroup")?.is_empty()
                && get("ExecMainCode")? == "1"
                && get("ExecMainStatus")? == "125",
            "resource actor is not the retained failed process",
        )?;
        label(&Value::String(get("InvocationID")?.to_owned()))?;
        let integer = |key| -> io::Result<u64> { get(key)?.parse().map_err(io::Error::other) };
        require(
            integer("ExecMainPID")? > 0
                && integer("ExecMainStartTimestampMonotonic")? > 0
                && integer("ExecMainStartTimestampMonotonic")?
                    < integer("ExecMainExitTimestampMonotonic")?
                && integer("ExecMainExitTimestampMonotonic")? <= started / 1000,
            "resource actor lacks prior real terminal timing",
        )?;
        if index < 2 {
            require(
                get("InvocationID")? == text(&a[2][0])?,
                "resource original invocation changed",
            )?;
        } else {
            let f = array(&a[3], 13)?;
            require(
                text(&f[0])? == format!("/run/systemd/transient/{}", units[index])
                    && get("FragmentPath")? == text(&f[0])?
                    && number(&f[1])? > 0
                    && number(&f[2])? > 0
                    && number(&f[3])? == u64::from(libc::S_IFREG | 0o644)
                    && number(&f[4])? == 0
                    && number(&f[5])? == 1
                    && number(&f[6])? > 0
                    && number(&f[6])? <= 65536,
                "resource retained manager fragment differs",
            )?;
            number(&f[7])?;
            number(&f[8])?;
            hex(&f[9], 64)?;
            hex(&f[11], 64)?;
            let helper = text(&f[10])?;
            require(
                Path::new(helper).is_absolute()
                    && helper
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b))
                    && number(&f[12])? > 0
                    && number(&f[12])? < started,
                "resource helper or bootstrap deadline differs",
            )?;
            let prefix = format!(
                "{{ path={helper} ; argv[]={helper} --bootstrap-deadline-ns {} ; ignore_errors=no ; ",
                number(&f[12])?
            );
            let command = get("ExecStart")?;
            let suffix = command
                .strip_prefix(&prefix)
                .and_then(|s| s.strip_suffix(" }"))
                .ok_or_else(|| io::Error::other("resource manager executable differs"))?;
            let fields: Vec<_> = suffix.split(" ; ").collect();
            require(
                fields.len() == 5
                    && fields
                        .iter()
                        .zip(["start_time", "stop_time", "pid", "code", "status"])
                        .all(|(f, k)| f.split_once('=').is_some_and(|(name, _)| name == k)),
                "resource manager command metadata differs",
            )?;
        }
    }
    Ok(())
}
