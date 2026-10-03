use serde::Deserialize;

use super::*;

pub(super) fn validate(
    context: &ResourceRecoveryContext,
    intent: &Value,
    accepted: &[Receipt],
    unix: &[Receipt],
    journal: &Receipt,
    accepted_prefix: &[u8],
    unix_prefix: &[u8],
) -> io::Result<()> {
    let i = array(intent, 11)?;
    let a = array(&i[6], 7)?;
    let u = array(&i[7], 8)?;
    let a_proofs = array(&a[4], 3)?;
    let u_proofs = array(&u[3], 4)?;
    for (index, receipt) in accepted.iter().enumerate() {
        receipt.matches_proof(&a_proofs[index], (index == 0).then_some(accepted_prefix))?;
    }
    for (index, receipt) in unix.iter().enumerate() {
        receipt.matches_proof(&u_proofs[index], (index == 0).then_some(unix_prefix))?;
    }
    journal.matches_proof(&u_proofs[3], None)?;
    let a_rows = objects(accepted_prefix, 3)?;
    let (before, started, failed) = (&a_rows[0], &a_rows[1], &a_rows[2]);
    fields(
        before,
        &["schema", "stage", "label", "root", "artifact", "files"],
    )?;
    fields(started, &["schema", "stage", "label", "observed"])?;
    fields(failed, &["schema", "stage", "label", "error"])?;
    for (row, stage) in [
        (before, "accepted_before_launch"),
        (started, "accepted_started"),
        (failed, "accepted_failed"),
    ] {
        require(
            row["schema"] == 1 && row["stage"] == stage && row["label"] == a[2],
            "resource accepted original stages differ",
        )?;
    }
    require(
        !text(&failed["error"])?.is_empty(),
        "resource accepted original failure absent",
    )?;
    let root = &before["root"];
    fields(root, &["device", "inode", "mode", "uid"])?;
    require(
        serde_json::json!([root["device"], root["inode"], root["mode"], root["uid"]])
            == context.accepted.identity,
        "resource original accepted root differs",
    )?;
    for (old, file) in array(&before["files"], 3)?.iter().zip(accepted) {
        let stat = array(&file.stat, 8)?;
        require(
            old == &serde_json::json!({"device":stat[0],"inode":stat[1],"mode":stat[2],"uid":stat[3]}),
            "resource original accepted file differs",
        )?;
    }
    let observed = &started["observed"];
    fields(observed, &["schema", "run", "artifact", "loader", "query"])?;
    require(
        observed["schema"] == "hermit-accepted-parent-startup-v1"
            && observed["artifact"] == before["artifact"],
        "resource original startup artifact differs",
    )?;
    let artifact = &before["artifact"];
    fields(
        artifact,
        &[
            "topology",
            "wire_format",
            "object_sha256",
            "library_sha256",
            "btf_sha256",
            "maps",
            "programs",
            "links",
        ],
    )?;
    fields(&artifact["topology"], &["kind", "contract_sha256"])?;
    let bytes = |v: &Value| -> io::Result<String> {
        let values: Vec<u8> = serde_json::from_value(v.clone())?;
        // Preserve validate_startup and ProviderTopology::validate even when
        // the old prefix and the new certificate agree on an invalid value.
        require(
            values.len() == 32 && values.iter().any(|byte| *byte != 0),
            "resource artifact digest is zero or malformed",
        )?;
        Ok(values.iter().map(|b| format!("{b:02x}")).collect())
    };
    require(
        artifact["topology"]["kind"] == "ftrace-v1"
            && artifact["wire_format"] == "abi9-copy5"
            && artifact["maps"] == 24
            && artifact["programs"] == 49
            && artifact["links"] == 49,
        "resource original accepted topology differs",
    )?;
    require(
        a[5] == serde_json::json!([
            artifact["wire_format"],
            bytes(&artifact["topology"]["contract_sha256"])?,
            bytes(&artifact["object_sha256"])?,
            bytes(&artifact["library_sha256"])?,
            bytes(&artifact["btf_sha256"])?,
            24,
            49,
            49
        ]),
        "resource accepted artifact binding differs",
    )?;
    let run: Vec<u8> = serde_json::from_value(observed["run"].clone())?;
    require(
        run.len() == 16
            && run.iter().any(|b| *b != 0)
            && text(&a[3])? == run.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        "resource original accepted run differs",
    )?;
    let actors = array(&i[8], 3)?;
    for (index, key) in ["loader", "query"].into_iter().enumerate() {
        let original = &observed[key];
        let actor = array(&actors[index], 4)?;
        require(
            original
                == &serde_json::json!({"unit":actor[0],"invocation":actor[2][0],"cgroup":actor[2][1],
            "device":actor[2][2],"inode":actor[2][3]}),
            "resource original accepted actor join differs",
        )?;
    }
    require(
        observed["loader"]["invocation"] != observed["query"]["invocation"]
            && (observed["loader"]["device"] != observed["query"]["device"]
                || observed["loader"]["inode"] != observed["query"]["inode"]),
        "resource accepted original actors alias",
    )?;
    let service = objects(&accepted[1].bytes, 2)?;
    let (pre, post) = (&service[0], &service[1]);
    for (row, phase) in [(pre, "before_close"), (post, "after_close")] {
        require(
            row["schema"] == "hermit-accepted-provider-terminal-v1"
                && row["phase"] == phase
                && row["run"] == observed["run"]
                && row["controller_terminal"] == true
                && row["requires_external_absence"] == true,
            "resource original accepted service identity differs",
        )?;
    }
    require(
        pre["failure"].is_null()
            && post["service_status"] == 125
            && post["close_error"].is_null()
            && post["socket_release"] == "pending_process_exit"
            && post["closed_ns"] == a[6],
        "resource accepted close is not supported failed shape",
    )?;
    let inventory_before = &array(&pre["inventories"], 1)?[0];
    let close = &array(&post["close_receipts"], 1)?[0];
    require(
        number(&close["incarnation"])? == u64::from_le_bytes(run[..8].try_into().unwrap())
            && close["close"]
                == serde_json::json!({"returned":0,"errno":null,"operation":"ap_close"})
            && close["unexpected_drop"] == false
            && close["requires_external_absence"] == true,
        "resource original physical close differs",
    )?;
    let accepted_ids = inventory(inventory_before, [24, 49, 49])?;
    require(
        inventory(&close["inventory"], [24, 49, 49])? == accepted_ids,
        "resource original accepted close inventory differs",
    )?;

    let u_rows = objects(unix_prefix, 2)?;
    let incarnation = u64::from_str_radix(&label(&u[2])?[..16], 16).map_err(io::Error::other)?;
    let unit = format!("hermit-unix-{}.service", text(&u[2])?);
    require(
        u_rows[0]
            == serde_json::json!({"schema":1,"stage":"before_launch","incarnation":incarnation,"loader_unit":unit}),
        "resource original Unix before-launch differs",
    )?;
    let failure = &u_rows[1];
    require(
        failure["schema"] == 1
            && failure["stage"] == "terminal_failed"
            && failure["units"] == serde_json::json!([unit, null])
            && failure["ids"].is_null()
            && failure["child_wait"] == "Exited(125)"
            && number(&failure["child_pid"])? > 0
            && number(&failure["child_pid"])? <= i32::MAX as u64
            && !text(&failure["error"])?.is_empty()
            && unix[1].bytes.is_empty()
            && unix[2].bytes.is_empty(),
        "resource original Unix failure differs",
    )?;
    let unix_ids = validate_journal(&journal.bytes, &u[4], &u[7], incarnation)?;
    let combined: Vec<_> = [(0, accepted_ids), (1, unix_ids)]
        .into_iter()
        .flat_map(|(domain, ids)| {
            ids.into_iter()
                .map(move |(kind, id)| serde_json::json!([domain, kind, id]))
        })
        .collect();
    require(
        i[9] == Value::Array(combined),
        "resource certificate inventory differs from original producers",
    )?;
    Ok(())
}
fn fields(value: &Value, expected: &[&str]) -> io::Result<()> {
    require(
        value.as_object().is_some_and(|object| {
            object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
        }),
        "resource original receipt fields differ",
    )
}
fn inventory(value: &Value, counts: [usize; 3]) -> io::Result<Vec<(u32, u32)>> {
    require(
        value["complete"] == true
            && value["count_invalid"] == false
            && value["status"]
                == serde_json::json!({"returned":0,"errno":null,"operation":"ap_identifiers"}),
        "resource original inventory call differs",
    )?;
    let mut ids = Vec::new();
    for row in list(&value["ids"])? {
        require(
            row.as_object().is_some_and(|r| r.len() == 2),
            "resource inventory entry shape differs",
        )?;
        ids.push((number(&row["kind"])?, number(&row["id"])?));
    }
    typed_ids(ids, counts)
}
fn typed_ids(mut ids: Vec<(u64, u64)>, counts: [usize; 3]) -> io::Result<Vec<(u32, u32)>> {
    let mut observed = [0usize; 3];
    for &(kind, id) in &ids {
        require(
            kind < 3 && id > 0 && id <= u32::MAX as u64,
            "resource original ID type differs",
        )?;
        observed[kind as usize] += 1;
    }
    ids.sort();
    require(
        observed == counts && ids.windows(2).all(|p| p[0] != p[1]),
        "resource original ID population differs",
    )?;
    Ok(ids
        .into_iter()
        .map(|(kind, id)| (kind as u32, id as u32))
        .collect())
}
fn validate_journal(
    data: &[u8],
    proof: &Value,
    directory: &Value,
    incarnation: u64,
) -> io::Result<Vec<(u32, u32)>> {
    let p = array(proof, 6)?;
    let d = array(directory, 4)?;
    require(
        !data.is_empty()
            && data.len().is_multiple_of(104)
            && data.len() <= UNIX_LIMIT
            && incarnation > 0
            && number(&p[0])? == incarnation
            && number(&p[1])? == data.len() as u64 / 104
            && hex(&p[2], 64)? == digest(data),
        "resource original journal bounds or hash differs",
    )?;
    let mut pins = std::collections::BTreeMap::new();
    let mut intents = std::collections::BTreeMap::new();
    let mut ids = Vec::new();
    let mut initial = Vec::new();
    let mut live = Vec::new();
    let mut failed = false;
    let mut admission_closed = false;
    for (index, row) in data.as_chunks::<104>().0.iter().enumerate() {
        let u64_at = |n| u64::from_le_bytes(row[n..n + 8].try_into().unwrap());
        let u32_at = |n| u32::from_le_bytes(row[n..n + 4].try_into().unwrap());
        let seq = u64_at(24);
        let phase = u32_at(52);
        let kind = u32_at(56);
        let id = u32_at(60);
        let error = i32::from_le_bytes(row[64..68].try_into().unwrap());
        require(
            u64_at(0) == 0x5547_5049_4e30_3031
                && u64_at(8) == incarnation
                && u64_at(16) == index as u64 + 1
                && u64_at(32) == number(&d[0])?
                && u64_at(40) == number(&d[1])?
                && u32_at(48) == 3
                && u32_at(68) == 0,
            "resource journal identity or ordinal differs",
        )?;
        require(
            (1..=18).contains(&phase)
                && !(13..=17).contains(&phase)
                && (index == 0) == (phase == 1),
            "resource journal phase differs",
        )?;
        let nul = row[72..]
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| io::Error::other("resource journal name lacks NUL"))?;
        require(
            row[72 + nul..].iter().all(|b| *b == 0),
            "resource journal padding differs",
        )?;
        let name = std::str::from_utf8(&row[72..72 + nul]).map_err(io::Error::other)?;
        require(name.is_ascii(), "resource journal name is not ASCII")?;
        match phase {
            2 | 3 => {
                require(
                    kind < 2 && id > 0 && error == 0 && seq == 0,
                    "resource journal pin identity differs",
                )?;
                let allowed = if kind == 0 {
                    (0..10).map(|n| format!("m{n:02}")).collect::<BTreeSet<_>>()
                } else {
                    (0..31).map(|n| format!("l{n:02}")).collect()
                };
                require(allowed.contains(name), "resource journal pin name differs")?;
                if phase == 2 {
                    require(
                        intents.insert(name.to_owned(), (kind, id)).is_none(),
                        "resource pin intent repeats",
                    )?;
                } else {
                    require(
                        intents.get(name) == Some(&(kind, id))
                            && pins.insert(name.to_owned(), (kind, id)).is_none(),
                        "resource pin commit lacks unique intent",
                    )?;
                }
            }
            18 => {
                require(
                    name.is_empty() && error == 0 && seq > 0,
                    "resource journal original ID shape differs",
                )?;
                ids.push((u64::from(kind), u64::from(id)));
            }
            8 | 9 => {
                require(
                    seq > 0 && name.is_empty() && kind == 0 && id == 0 && error == 0,
                    "resource journal initial row differs",
                )?;
                if phase == 8 {
                    initial.push(seq);
                } else {
                    live.push(seq);
                }
            }
            10 => {
                require(
                    seq > 0
                        && name.is_empty()
                        && kind == 0
                        && id == 0
                        && matches!(error, libc::EPROTO | libc::EPIPE),
                    "resource journal failure differs",
                )?;
                failed = true;
            }
            _ => {
                require(
                    name.is_empty() && kind == 0 && id == 0 && error == 0,
                    "resource journal payload differs",
                )?;
                admission_closed |= phase == 12;
            }
        }
    }
    let original = typed_ids(ids, [10, 31, 31])?;
    require(
        pins.len() == 41
            && pins == intents
            && !initial.is_empty()
            && initial.len() <= 256
            && initial == live
            && initial.iter().collect::<BTreeSet<_>>().len() == initial.len()
            && failed
            && admission_closed,
        "resource journal original population is incomplete",
    )?;
    for &(kind, id) in pins.values() {
        require(
            original.contains(&(if kind == 0 { 0 } else { 2 }, id)),
            "resource original pin lacks inventory identity",
        )?;
    }
    let pin_rows: Vec<_> = pins
        .into_iter()
        .map(|(name, (kind, id))| serde_json::json!([name, kind, id]))
        .collect();
    require(
        p[3] == serde_json::to_value(initial)?
            && p[4] == Value::Array(pin_rows)
            && p[5] == serde_json::to_value(&original)?,
        "resource journal certificate differs from actual bytes",
    )?;
    Ok(original)
}
fn objects(bytes: &[u8], count: usize) -> io::Result<Vec<Value>> {
    raw_rows(bytes, count)?
        .into_iter()
        .map(|r| {
            let Unique(value) = serde_json::from_slice(r)?;
            require(value.is_object(), "resource original row is not an object")?;
            Ok(value)
        })
        .collect()
}

// Preserve duplicate-key refusal while reading the old object-based protocol.
// New authority rows are fixed arrays; duplicates cannot be collapsed there.
struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Unique;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON with unique object keys")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Unique, E> {
                Err(E::custom("floating point is not a receipt integer"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Unique, A::Error> {
                let mut values = Vec::new();
                while let Some(Unique(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(Unique(Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Unique, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom("duplicate original receipt key"));
                    }
                    let Unique(value) = map.next_value()?;
                    values.insert(key, value);
                }
                Ok(Unique(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
