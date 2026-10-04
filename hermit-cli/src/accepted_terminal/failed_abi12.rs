//! Authenticate a failed ABI12 child125 source for a separate physical certificate.
//! This cannot publish ordinary success or change the original failed evidence.

use super::*;

pub(crate) fn failed_abi12_resource_source(
    prefix: &[u8],
    stdout: &[u8],
    label: &str,
    root: &crate::unix_guard_package::RecoveryDirectoryIdentity,
    files: [&File; 3],
) -> io::Result<IncompleteResourceSource> {
    receipt_label(label)?;
    let text = std::str::from_utf8(prefix).map_err(io::Error::other)?;
    let rows: Vec<_> = text.lines().collect();
    if rows.len() != 3 || !text.ends_with('\n') {
        return Err(io::Error::other(
            "accepted failed ABI12 source row population differs",
        ));
    }
    // Reject duplicate keys before the existing typed original parsers run.
    for row in &rows {
        serde_json::from_str::<UniqueReceiptJson>(row)?;
    }
    let before: BeforeReceipt = serde_json::from_str(rows[0])?;
    let started: StartedReceipt = serde_json::from_str(rows[1])?;
    if before.schema != 1
        || started.schema != 1
        || before.stage != "accepted_before_launch"
        || started.stage != "accepted_started"
        || before.label != label
        || started.label != label
        || &before.root != root
    {
        return Err(io::Error::other(
            "accepted failed ABI12 source identity differs",
        ));
    }
    let failed: UniqueReceiptJson = serde_json::from_str(rows[2])?;
    if !failed.0.as_object().is_some_and(|fields| fields.len() == 4)
        || failed.0["schema"].as_u64() != Some(1)
        || failed.0["stage"] != "accepted_failed"
        || failed.0["label"] != label
        || failed.0["error"]
            != "accepted terminal: accepted service wrapper failed exit status: 125; raw logs retained; child_wait=Some(32000)"
    {
        return Err(io::Error::other(
            "accepted failed ABI12 original failure differs",
        ));
    }
    validate_startup(&started.observed, &before.artifact)?;
    if !matches!(
        before.artifact.topology,
        detcore::network_runtime::ProviderTopology::FtraceV1 { contract_sha256 }
            if contract_sha256 == [213, 243, 121, 246, 28, 136, 214, 210, 216, 103, 149, 150, 248, 21, 240, 170, 246, 92, 59, 84, 155, 211, 182, 120, 132, 61, 207, 32, 176, 239, 182, 173]
    ) || before.artifact.wire_format != detcore::network_runtime::ProviderWireFormat::Abi12Copy5
    {
        return Err(io::Error::other(
            "accepted failed ABI12 source topology differs",
        ));
    }
    for (file, original) in files.into_iter().zip(&before.files) {
        if &receipt_identity(file)? != original {
            return Err(io::Error::other(
                "accepted failed ABI12 original file differs",
            ));
        }
    }
    let stdout = std::str::from_utf8(stdout).map_err(io::Error::other)?;
    strict_service_transcript(stdout)?;
    let first: UniqueReceiptJson = serde_json::from_str(stdout.lines().next().unwrap())?;
    let inventories = first.0["inventories"]
        .as_array()
        .filter(|v| v.len() == 1)
        .ok_or_else(|| io::Error::other("accepted failed ABI12 inventory population differs"))?;
    let mut original_ids = inventory(&inventories[0])?;
    validate_ids(&original_ids, [26, 49, 49])?;
    original_ids.sort_unstable();
    let second: UniqueReceiptJson = serde_json::from_str(stdout.lines().nth(1).unwrap())?;
    for (row, phase) in [(&first.0, "before_close"), (&second.0, "after_close")] {
        if row["schema"] != "hermit-accepted-provider-terminal-v1"
            || row["phase"] != phase
            || row["run"] != serde_json::json!(started.observed.run)
            || row["controller_terminal"] != true
            || row["requires_external_absence"] != true
        {
            return Err(io::Error::other(
                "accepted failed ABI12 service identity differs",
            ));
        }
    }
    if second.0["service_status"].as_i64() != Some(125)
        || second.0["socket_release"] != "pending_process_exit"
        || !second.0["close_receipts"]
            .as_array()
            .is_some_and(|v| v.len() == 1)
    {
        return Err(io::Error::other(
            "accepted failed ABI12 service close shape differs",
        ));
    }
    let closed_ns = second.0["closed_ns"]
        .as_u64()
        .filter(|v| *v > 0)
        .ok_or_else(|| io::Error::other("accepted failed ABI12 close time absent"))?;
    let close = &second.0["close_receipts"][0];
    if close["incarnation"].as_u64()
        != Some(u64::from_le_bytes(
            started.observed.run[..8].try_into().unwrap(),
        ))
        || close["unexpected_drop"] != false
        || close["requires_external_absence"] != true
    {
        return Err(io::Error::other(
            "accepted failed ABI12 physical close identity differs",
        ));
    }
    let mut after_ids = inventory(&close["inventory"])?;
    validate_ids(&after_ids, [26, 49, 49])?;
    after_ids.sort_unstable();
    if after_ids != original_ids {
        return Err(io::Error::other(
            "accepted failed ABI12 closed inventory differs",
        ));
    }

    if close["close"] != serde_json::json!({"returned":0,"errno":null,"operation":"ap_close"}) {
        return Err(io::Error::other(
            "accepted failed ABI12 close operation differs",
        ));
    }
    for inventory in [&inventories[0], &close["inventory"]] {
        if !inventory.as_object().is_some_and(|v| v.len() == 4)
            || inventory["status"]
                != serde_json::json!({"returned":0,"errno":null,"operation":"ap_identifiers"})
            || !inventory["ids"].as_array().is_some_and(|ids| {
                ids.iter()
                    .all(|id| id.as_object().is_some_and(|v| v.len() == 2))
            })
        {
            return Err(io::Error::other(
                "accepted failed ABI12 inventory operation differs",
            ));
        }
    }
    let actors =
        [started.observed.loader, started.observed.query].map(|actor| IncompleteResourceActor {
            unit: actor.unit,
            invocation: actor.invocation,
            cgroup: actor.cgroup,
            device: actor.device,
            inode: actor.inode,
        });
    Ok(IncompleteResourceSource {
        run: started.observed.run,
        artifact: before.artifact,
        actors,
        original_ids,
        closed_ns,
    })
}
