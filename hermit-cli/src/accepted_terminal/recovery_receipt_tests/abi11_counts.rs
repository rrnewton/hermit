//! Receipt grammar controls; no BPF load or native cleanup is simulated as proof.
use detcore::network_runtime::ProviderTopology;
use detcore::network_runtime::ProviderWireFormat;

use super::*;

const LEGACY: [ProviderWireFormat; 6] = [
    ProviderWireFormat::Abi7Copy4,
    ProviderWireFormat::Abi8Copy5,
    ProviderWireFormat::Abi9Copy4,
    ProviderWireFormat::Abi9Copy5,
    ProviderWireFormat::Abi10Copy4,
    ProviderWireFormat::Abi10Copy5,
];
const CURRENT: [ProviderWireFormat; 2] = [
    ProviderWireFormat::Abi11Copy4,
    ProviderWireFormat::Abi11Copy5,
];
const OLD_COUNTS: [[usize; 3]; 3] = [[23, 46, 58], [23, 44, 44], [24, 49, 49]];
const NEW_COUNTS: [[usize; 3]; 3] = [[24, 46, 58], [24, 44, 44], [25, 49, 49]];

fn selected(wire: ProviderWireFormat, topology: usize, counts: [usize; 3]) -> ProviderArtifact {
    let mut artifact = artifact();
    artifact.wire_format = wire;
    artifact.topology = match topology {
        0 => ProviderTopology::ClassicV40,
        1 => ProviderTopology::GroupedV1 {
            contract_sha256: [9; 32],
        },
        2 => ProviderTopology::FtraceV1 {
            contract_sha256: [9; 32],
        },
        _ => unreachable!(),
    };
    [artifact.maps, artifact.programs, artifact.links] = counts;
    artifact
}

fn populated(
    profile: &ProviderArtifact,
) -> (tempfile::TempDir, AcceptedRecovery, StartupReceipt, Value) {
    // Reuse unchanged original actor/run/timing grammar. The new recovery
    // actually initializes its own files with the selected artifact.
    let (_seed_temp, _seed, mut startup, mut terminal) = fixture();
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = crate::unix_guard_package::RecoveryDeploymentRoot::open_at(temp.path()).unwrap();
    let mut recovery = AcceptedRecovery::retain(root, "1a".repeat(16));
    recovery.initialize(profile.clone()).unwrap();
    startup.artifact = profile.clone();
    let counts = [profile.maps, profile.programs, profile.links];
    let ids: Vec<_> = counts
        .iter()
        .enumerate()
        .flat_map(|(kind, count)| (1..=*count).map(move |id| (kind as u32, id as u32)))
        .collect();
    let inventory_ids = ids
        .iter()
        .map(|(kind, id)| serde_json::json!({"kind":kind,"id":id}))
        .collect::<Vec<_>>();
    let mut service: Vec<Value> = terminal["service_stdout"]
        .as_str()
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    service[0]["inventories"][0]["ids"] = serde_json::json!(inventory_ids);
    service[1]["close_receipts"][0]["inventory"]["ids"] = serde_json::json!(inventory_ids);
    let stdout = service
        .iter()
        .map(|row| serde_json::to_string(row).unwrap() + "\n")
        .collect::<String>();
    recovery.files[1]
        .as_mut()
        .unwrap()
        .write_all(stdout.as_bytes())
        .unwrap();
    recovery.files[1].as_ref().unwrap().sync_all().unwrap();
    terminal["service_stdout"] = stdout.into();
    terminal["original_ids"] = serde_json::json!(ids);
    terminal["counts"] = serde_json::json!(counts);
    terminal["readback"]["request"]["ids"] = serde_json::json!(ids);
    terminal["readback"]["request"]["counts"] = serde_json::json!(counts);
    (temp, recovery, startup, terminal)
}

fn population_refused(profile: ProviderArtifact) {
    let (_temp, mut recovery, startup, terminal) = populated(&profile);
    let expected = "accepted receipt artifact population or identity differs";
    assert_eq!(
        validate_startup(&startup, &profile)
            .unwrap_err()
            .to_string(),
        expected
    );
    // Jointly changed expected+observed artifacts cannot bypass the hard gate,
    // even when every terminal inventory/count/readback agrees with that lie.
    let typed: TerminalReceipt = serde_json::from_value(terminal.clone()).unwrap();
    assert_eq!(
        validate_terminal(&startup, &typed, typed.service_stdout.as_bytes(), b"")
            .unwrap_err()
            .to_string(),
        expected
    );
    assert_eq!(
        recovery
            .started(serde_json::to_value(startup).unwrap())
            .unwrap_err()
            .to_string(),
        expected
    );
    assert!(validate_accepted_receipt(&recovery.root, &recovery.label, &profile).is_err());
}

#[test]
fn abi11_startup_and_complete_normal_receipts_accept_all_exact_topologies() {
    for wire in CURRENT {
        for (topology, counts) in NEW_COUNTS.into_iter().enumerate() {
            let profile = selected(wire, topology, counts);
            let (_temp, mut recovery, startup, terminal) = populated(&profile);
            validate_startup(&startup, &profile).unwrap();
            recovery
                .started(serde_json::to_value(startup).unwrap())
                .unwrap();
            recovery.finish(terminal).unwrap();
            let readback =
                validate_accepted_receipt(&recovery.root, &recovery.label, &profile).unwrap();
            assert_eq!(readback.counts, counts);
            assert_eq!(readback.original_ids.len(), counts.iter().sum::<usize>());
            assert!(recovery.finished);
            assert!(!recovery.failed);
        }
    }
}

#[test]
fn every_older_wire_retains_its_exact_original_startup_and_terminal_population() {
    for wire in LEGACY {
        for (topology, counts) in OLD_COUNTS.into_iter().enumerate() {
            let profile = selected(wire, topology, counts);
            let (_temp, mut recovery, startup, terminal) = populated(&profile);
            validate_startup(&startup, &profile).unwrap();
            recovery
                .started(serde_json::to_value(startup).unwrap())
                .unwrap();
            recovery.finish(terminal).unwrap();
            let readback =
                validate_accepted_receipt(&recovery.root, &recovery.label, &profile).unwrap();
            assert_eq!(readback.counts, counts);
            assert_eq!(readback.original_ids.len(), counts.iter().sum::<usize>());
        }
    }
}

#[test]
fn jointly_crossed_abi_counts_and_wrong_program_or_link_counts_refuse() {
    for topology in 0..3 {
        for wire in CURRENT {
            population_refused(selected(wire, topology, OLD_COUNTS[topology]));
        }
        for wire in LEGACY {
            population_refused(selected(wire, topology, NEW_COUNTS[topology]));
        }
        for (wires, counts) in [
            (&CURRENT[..], NEW_COUNTS[topology]),
            (&LEGACY[..], OLD_COUNTS[topology]),
        ] {
            for &wire in wires {
                for field in 0..3 {
                    for delta in [-1i32, 1] {
                        let mut wrong = counts;
                        wrong[field] = (wrong[field] as i32 + delta) as usize;
                        population_refused(selected(wire, topology, wrong));
                    }
                }
            }
        }
    }
}
