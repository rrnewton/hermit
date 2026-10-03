//! A distinct admission-only proof for an incomplete parent after provider close.
//! Existing paired certificates and all normal successful-run validators remain
//! separate. No missing terminal wait or historical close deadline is invented.

use super::*;
use crate::accepted_terminal::IncompleteResourceActor;

const PRODUCER: &[u8] = include_bytes!("../../../scripts/accepted_resource_recovery.py");
const DEPENDENCY: &str = "b6c42c56c88f45cd14dce607fbb673a4ccae3fc4b267f98d423d3858f09565a9";
const INTENT: &str = "hermit-accepted-resource-intent-v1";
const RESULT: &str = "hermit-accepted-resource-result-v1";

pub(super) fn resolve(
    context: &ResourceRecoveryContext,
    actual: BorrowedFd<'_>,
    selected: &str,
) -> io::Result<()> {
    let root = &context.accepted;
    require(
        descriptor_identity(actual)? == root.identity,
        "accepted-only admission root differs",
    )?;
    label(&Value::String(selected.to_owned()))?;
    let files = ["terminal.jsonl", "stdout.log", "stderr.log"]
        .map(|role| root.read(&format!("accepted-b1-{selected}.{role}"), ACCEPTED_LIMIT));
    let [terminal, stdout, stderr] = files;
    let files = [terminal?, stdout?, stderr?];
    let rows = raw_rows(&files[0].bytes, 4)?;
    let intent: Value = serde_json::from_slice(rows[2])?;
    let result: Value = serde_json::from_slice(rows[3])?;
    let i = array(&intent, 12)?;
    let r = array(&result, 12)?;
    require(
        text(&i[0])? == INTENT && text(&r[0])? == RESULT,
        "accepted-only protocol differs",
    )?;
    let prefix = &files[0].bytes[..rows[0].len() + rows[1].len()];
    let metadata = root.file.metadata()?;
    let identity = crate::unix_guard_package::RecoveryDirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        uid: metadata.uid(),
    };
    let source = crate::accepted_terminal::incomplete_resource_source(
        prefix,
        &files[1].bytes,
        selected,
        &identity,
        [&files[0].file, &files[1].file, &files[2].file],
    )?;
    let a = array(&i[7], 7)?;
    require(
        Path::new(text(&a[0])?) == root.path
            && a[1] == root.identity
            && label(&a[2])? == selected
            && hex(&a[3], 32)? == hex_bytes(&source.run),
        "accepted-only configured source differs",
    )?;
    let proofs = array(&a[4], 3)?;
    for (index, file) in files.iter().enumerate() {
        file.matches_proof(&proofs[index], (index == 0).then_some(prefix))?;
    }
    let contract = match source.artifact.topology {
        detcore::network_runtime::ProviderTopology::FtraceV1 { contract_sha256 } => contract_sha256,
        _ => return Err(io::Error::other("accepted-only source topology differs")),
    };
    require(
        a[5] == serde_json::json!([
            "abi9-copy5",
            hex_bytes(&contract),
            hex_bytes(&source.artifact.object_sha256),
            hex_bytes(&source.artifact.library_sha256),
            hex_bytes(&source.artifact.btf_sha256),
            24,
            49,
            49
        ]) && number(&a[6])? == source.closed_ns,
        "accepted-only source artifact or close differs",
    )?;
    let ids: Vec<_> = source
        .original_ids
        .iter()
        .map(|&(kind, id)| serde_json::json!([0, kind, id]))
        .collect();
    require(
        i[9] == Value::Array(ids.clone())
            && ids.len() == 122
            && hex(&i[10], 64)? == digest(&serde_json::to_vec(&i[9])?)
            && text(&i[11])? == "incomplete-parent-after-provider-close",
        "accepted-only original inventory or profile differs",
    )?;
    let producer = digest(PRODUCER);
    require(
        hex(&i[5], 64)? == producer
            && r[10] == i[5]
            && hex(&i[6], 64)? == DEPENDENCY
            && r[11] == i[6]
            && digest(super::PRODUCER) == DEPENDENCY
            && hex(&r[1], 64)? == digest(rows[2]),
        "accepted-only producer or raw intent differs",
    )?;
    let boot = text(&i[1])?;
    require(
        uuid::Uuid::parse_str(boot).is_ok_and(|id| id.to_string() == boot)
            && boot == std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim()
            && r[2] == i[1]
            && number(&i[4])? == u64::from(unsafe { libc::getuid() }),
        "accepted-only current boot or owner differs",
    )?;
    let started = number(&i[2])?;
    let action_deadline = number(&i[3])?;
    let verification = number(&r[3])?;
    let deadline = number(&r[4])?;
    let final_ns = number(&r[8])?;
    require(
        started > 0
            && started.checked_add(5_000_000_000) == Some(action_deadline)
            && source
                .closed_ns
                .checked_add(1_000_000_000)
                .is_some_and(|old| old <= started)
            && started <= verification
            && verification <= final_ns
            && final_ns <= action_deadline
            && verification.checked_add(1_000_000_000) == Some(deadline)
            && final_ns <= deadline,
        "accepted-only fresh verification interval differs",
    )?;
    let pre = actors(&i[8], &source.actors, 1, started)?;
    let scanner = array(&r[5], 6)?;
    require(
        number(&scanner[0])? > 0
            && number(&scanner[0])? <= i32::MAX as u64
            && number(&scanner[1])? > 0
            && number(&scanner[2])? == 0
            && number(&scanner[3])? == 0
            && hex(&scanner[4], 64)?.bytes().any(|b| b != b'0')
            && scanner[5] == i[5]
            && number(&r[6])? == 0,
        "accepted-only actual scanner or wait differs",
    )?;
    let mut previous = verification;
    for scan in array(&r[7], 2)? {
        let scan = array(scan, 3)?;
        let begin = number(&scan[0])?;
        let end = number(&scan[1])?;
        require(
            previous <= begin && begin <= end && end <= final_ns,
            "accepted-only scan chronology differs",
        )?;
        for (answer, id) in array(&scan[2], 122)?.iter().zip(&ids) {
            let answer = array(answer, 4)?;
            require(
                answer[..3] == array(id, 3)?[..] && number(&answer[3])? == libc::ENOENT as u64,
                "accepted-only scan lacks an original absent ID",
            )?;
        }
        previous = end;
    }
    let post = actors(&r[9], &source.actors, previous, final_ns)?;
    require(pre == post, "accepted-only original actors changed")?;
    for actor in &source.actors {
        match actor.cgroup.symlink_metadata() {
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
            _ => return Err(io::Error::other("accepted-only actor absence is unknown")),
        }
    }
    root.recheck()?;
    for file in &files {
        file.recheck(root)?;
    }
    Ok(())
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn actors(
    value: &Value,
    original: &[IncompleteResourceActor; 2],
    first: u64,
    last: u64,
) -> io::Result<Value> {
    let mut stable = Vec::new();
    let mut previous = first;
    for (row, old) in array(value, 2)?.iter().zip(original) {
        let row = array(row, 7)?;
        let before = number(&row[4])?;
        let after = number(&row[5])?;
        let expected_cgroup = format!("/sys/fs/cgroup/system.slice/{}", old.unit);
        require(
            old.cgroup == Path::new(&expected_cgroup),
            "accepted-only original actor cgroup profile differs",
        )?;
        require(
            text(&row[0])? == old.unit
                && row[1] == serde_json::json!([old.invocation, old.cgroup, old.device, old.inode])
                && number(&row[3])? == 0
                && number(&row[6])? == libc::ENOENT as u64
                && previous <= before
                && before <= after
                && after <= last,
            "accepted-only original actor or observation differs",
        )?;
        previous = after;
        let expected = serde_json::json!([
            ["ActiveState", "inactive"],
            ["ControlGroup", ""],
            ["ExecMainStatus", "0"],
            ["Id", old.unit],
            ["InvocationID", ""],
            ["LoadState", "not-found"],
            ["MainPID", "0"],
            ["Result", "success"],
            ["SubState", "dead"]
        ]);
        require(
            row[2] == expected,
            "accepted-only manager is not the collected original unit",
        )?;
        stable.push(serde_json::json!([row[0], row[1], row[2], row[3], row[6]]));
    }
    Ok(Value::Array(stable))
}

#[cfg(test)]
mod tests;
