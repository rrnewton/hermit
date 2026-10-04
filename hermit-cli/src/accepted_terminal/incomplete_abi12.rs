//! Authenticate an incomplete ABI12 parent after its exact service close.
//! This source cannot construct a normal terminal or supply a missing wait.

use super::*;

pub(crate) fn incomplete_abi12_resource_source(
    prefix: &[u8],
    stdout: &[u8],
    label: &str,
    root: &crate::unix_guard_package::RecoveryDirectoryIdentity,
    files: [&File; 3],
) -> io::Result<IncompleteResourceSource> {
    receipt_label(label)?;
    let text = std::str::from_utf8(prefix).map_err(io::Error::other)?;
    let rows: Vec<_> = text.lines().collect();
    if rows.len() != 2 || !text.ends_with('\n') {
        return Err(io::Error::other(
            "accepted incomplete ABI12 source row population differs",
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
            "accepted incomplete ABI12 source identity differs",
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
            "accepted incomplete ABI12 source topology differs",
        ));
    }
    for (file, original) in files.into_iter().zip(&before.files) {
        if &receipt_identity(file)? != original {
            return Err(io::Error::other(
                "accepted incomplete ABI12 original file differs",
            ));
        }
    }
    let stdout = std::str::from_utf8(stdout).map_err(io::Error::other)?;
    strict_service_transcript(stdout)?;
    let first: UniqueReceiptJson = serde_json::from_str(stdout.lines().next().unwrap())?;
    let inventories = first.0["inventories"]
        .as_array()
        .filter(|v| v.len() == 1)
        .ok_or_else(|| {
            io::Error::other("accepted incomplete ABI12 inventory population differs")
        })?;
    let mut original_ids = inventory(&inventories[0])?;
    validate_ids(&original_ids, [26, 49, 49])?;
    original_ids.sort_unstable();
    let closed_ns = closed_inventory(stdout, started.observed.run, &original_ids)?;
    let second: UniqueReceiptJson = serde_json::from_str(stdout.lines().nth(1).unwrap())?;
    let close = &second.0["close_receipts"][0];
    if close["close"] != serde_json::json!({"returned":0,"errno":null,"operation":"ap_close"}) {
        return Err(io::Error::other(
            "accepted incomplete ABI12 close operation differs",
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
                "accepted incomplete ABI12 inventory operation differs",
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
