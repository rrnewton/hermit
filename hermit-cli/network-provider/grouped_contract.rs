/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
//! Exact nonclassic topology. Grouped-v1 owns one physical named event;
//! ftrace-v1 retains the same 17-role compatibility table while owning no
//! tracefs definition or perf-event link.
use super::Contract;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupedSite {
    pub role: u32,
    pub symbol: String,
    pub address: u64,
    pub offset: u64,
    pub cookie: u64,
}
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReceiveSite {
    pub cookie: u64,
    pub symbol: String,
    pub address: u64,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupedEvent {
    pub version: u32,
    pub program: String,
    pub cookie: u64,
    pub anchor_symbol: String,
    pub anchor_address: u64,
    pub sites: Vec<GroupedSite>,
    pub receive_entry: Vec<ReceiveSite>,
    pub receive_return: Vec<ReceiveSite>,
}

const CLASSIC: &[(u32, &str, u64, u64, u64)] = &[
    (1, "__sys_connect", 0xffffffff8206bf70, 0x1c, 9),
    (2, "__sys_connect", 0xffffffff8206bf70, 0x41, 3),
    (3, "__sys_connect", 0xffffffff8206bf70, 0x46, 5),
    (4, "__sys_accept4", 0xffffffff82197db0, 0x21, 8),
    (5, "fdget_raw", 0xffffffff81faaca0, 0x7c, 13),
    (6, "fdget_raw", 0xffffffff81faaca0, 0x5, 12),
    (7, "__x64_sys_read", 0xffffffff81fae890, 0x13, 14),
    (8, "fdget_pos", 0xffffffff81faede0, 0x96, 15),
    (9, "fdget_pos", 0xffffffff81faede0, 0xfa, 16),
    (10, "do_epoll_ctl", 0xffffffff81fb1ca0, 0x23, 18),
    (11, "do_epoll_ctl", 0xffffffff81fb1ca0, 0x37, 19),
    (12, "__skb_datagram_iter", 0xffffffff81fb8970, 0x64, 5),
    (13, "__skb_datagram_iter", 0xffffffff81fb8970, 0x69, 6),
    (14, "__skb_datagram_iter", 0xffffffff81fb8970, 0x26b, 7),
    (15, "__skb_datagram_iter", 0xffffffff81fb8970, 0x270, 8),
    (16, "inet_recvmsg", 0xffffffff82355dc0, 0x1b, 32),
    (17, "inet6_recvmsg", 0xffffffff820524a0, 0x1b, 33),
];
const RECEIVE: &[(u64, &str, u64)] = &[
    (1, "inet_recvmsg", 0xffffffff82355dc0),
    (2, "inet6_recvmsg", 0xffffffff820524a0),
    (3, "unix_stream_recvmsg", 0xffffffff8215a110),
    (4, "skb_copy_datagram_iter", 0xffffffff81fb85d0),
    (5, "tcp_recvmsg", 0xffffffff81fb7350),
    (6, "tcp_recvmsg_locked", 0xffffffff8206dfe0),
    (7, "unix_stream_read_generic", 0xffffffff8215a180),
    (8, "tcp_splice_read", 0xffffffff81e68b10),
    (9, "tcp_read_sock", 0xffffffff81e69130),
    (10, "tcp_read_sock_noack", 0xffffffff81e69310),
    (11, "__tcp_read_sock", 0xffffffff81e69150),
    (12, "tcp_read_skb", 0xffffffff81e69320),
    (13, "tcp_read_done", 0xffffffff81e69440),
    (14, "tcp_zerocopy_receive", 0xffffffff81e6a2e0),
    (15, "tcp_bpf_recvmsg", 0xffffffff81e9c2c0),
    (16, "tcp_bpf_recvmsg_parser", 0xffffffff81e9c510),
    (17, "unix_stream_splice_read", 0xffffffff81eb56d0),
    (18, "unix_stream_read_skb", 0xffffffff81eb5750),
    (19, "unix_read_skb", 0xffffffff81eb5df0),
    (20, "unix_bpf_recvmsg", 0xffffffff81eb6f10),
];
const SOURCES: &[&str] = &[
    "driver-grouped.c",
    "grouped-api.h",
    "grouped-driver.h",
    "grouped-io.c",
    "grouped-io.h",
    "grouped-owner.c",
    "grouped-owner.h",
    "grouped-probes.bpf.h",
    "grouped-probes.h",
    "grouped-target.h",
    "provider-grouped.bpf.c",
    "stream-membership.bpf.h",
    "grouped_contract.rs",
    "accepted-classic-v40-contract.json",
];

const FRONTIER_SOURCES: &[&str] = &[
    "accepted-grouped-v4-contract.json",
    "fd-call-shared.bpf.h",
    "fd-journal.bpf.h",
    "stream-frontier.h",
    "grouped-broker-bridge.c",
    "grouped-broker-bridge.h",
    "grouped-keeper-wire.c",
    "grouped-keeper-wire.h",
    "grouped-guardian-bootstrap.c",
    "grouped-guardian-bootstrap.h",
    "grouped-keeper-dual.c",
    "grouped-keeper-dual.h",
    "grouped-adoption-wire.c",
    "grouped-adoption-wire.h",
    "grouped-cleanup-bridge.c",
    "grouped-cleanup-bridge.h",
];
const FTRACE_SOURCES: &[&str] = &[
    "ftrace-coverage.h",
    "driver-grouped.c",
    "grouped-driver.h",
    "grouped-io.h",
    "grouped-owner.h",
    "grouped-probes.bpf.h",
    "grouped-probes.h",
    "grouped-target.h",
    "provider-grouped.bpf.c",
    "stream-membership.bpf.h",
    "grouped_contract.rs",
    "accepted-classic-v40-contract.json",
    "accepted-grouped-v4-contract.json",
    "fd-call-shared.bpf.h",
    "fd-journal.bpf.h",
    "stream-frontier.h",
    "stream-copy-custody.inc",
    "stream-copy-problem.inc",
    "stream-copy-unit-enter.inc",
    "stream-copy-emit.inc",
    "stream-copy-unit-exit.inc",
    "stream-copy-fault.h",
    "stream-copy-fault.inc",
    "fd-session-dispatch.inc",
];

pub fn validate(contract: &Contract) -> Result<()> {
    let group = contract.grouped_event.as_ref().expect("explicit caller");
    ensure!(
        (if contract.ftrace_only {
            matches!(contract.accepted_copy_version(), Ok(5))
        } else {
            matches!(contract.accepted_copy_version(), Ok(4 | 5))
        })
            && contract.maps == if contract.ftrace_only { 24 } else { 23 }
            && contract.programs == if contract.ftrace_only { 49 } else { 44 }
            && contract.links == if contract.ftrace_only { 49 } else { 44 }
            && contract.shared_links.is_empty(),
        "unsupported grouped inventory or ABI"
    );
    ensure!(
        ((contract.ftrace_only && group.version == 2
            && group.program == "ftrace-replacements-v1" && group.cookie == 0) ||
         (!contract.ftrace_only && group.version == 1
            && group.program == "fd_connect_post_fdget"
            && group.cookie == 0x4845524d49544731))
            && group.anchor_symbol == "__sys_connect"
            && group.anchor_address == 0xffffffff8206bf70,
        "unsupported grouped anchor or program"
    );
    ensure!(
        group.sites.len() == CLASSIC.len()
            && group.sites.iter().zip(CLASSIC).all(
                |(site, &(role, symbol, address, offset, cookie))| site.role == role
                    && site.symbol == symbol
                    && site.address == address
                    && site.offset == offset
                    && site.cookie == cookie
            ),
        "missing, reordered or mismatched grouped physical site"
    );
    ensure!(
        group.receive_entry.len() == RECEIVE.len()
            && group.receive_entry.iter().zip(RECEIVE).all(
                |(site, &(cookie, symbol, address))| site.cookie == cookie
                    && site.symbol == symbol
                    && site.address == address
            )
            && group.receive_return.as_slice() == &group.receive_entry[..4],
        "missing or mismatched receive membership entry/return"
    );
    // This immutable historical fixture also keeps every original parser
    // count/site test intact. Its exact legacy contract is validated normally.
    let legacy = Contract::parse(include_bytes!("accepted-classic-v40-contract.json"))?;
    let mut sources = legacy.source_files;
    if contract.ftrace_only {
        sources.extend(FTRACE_SOURCES.iter().map(|name| (*name).to_owned()));
    } else {
        sources.extend(SOURCES.iter().map(|name| (*name).to_owned()));
    }
    if contract.accepted_copy_version()? == 5 && !contract.ftrace_only {
        sources.extend(FRONTIER_SOURCES.iter().map(|name| (*name).to_owned()));
    }
    ensure!(
        contract.source_files == sources,
        "grouped source closure differs"
    );
    let mut hooks = legacy.hooks;
    if contract.ftrace_only {
        hooks.insert("fixup_exception".to_owned(), (vec![8, 4, 8, 8], 4, 4));
    }
    hooks.insert("tcp_recvmsg".to_owned(), (vec![8, 8, 8, 4], 4, 4));
    hooks.insert("unix_stream_read_generic".to_owned(), (vec![8, 1], 2, 4));
    hooks.insert("tcp_splice_read".to_owned(), (vec![8, 8, 8, 8, 4], 5, 8));
    hooks.insert("tcp_read_sock".to_owned(), (vec![8, 8, 8], 3, 4));
    hooks.insert(
        "tcp_read_sock_noack".to_owned(),
        (vec![8, 8, 8, 1, 8], 5, 4),
    );
    hooks.insert("__tcp_read_sock".to_owned(), (vec![8, 8, 8, 1, 8], 5, 4));
    hooks.insert("tcp_read_skb".to_owned(), (vec![8, 8], 2, 4));
    hooks.insert("tcp_read_done".to_owned(), (vec![8, 8], 2, 0));
    hooks.insert("tcp_zerocopy_receive".to_owned(), (vec![8, 8, 8], 3, 4));
    hooks.insert("tcp_bpf_recvmsg".to_owned(), (vec![8, 8, 8, 4], 4, 4));
    hooks.insert(
        "tcp_bpf_recvmsg_parser".to_owned(),
        (vec![8, 8, 8, 4], 4, 4),
    );
    hooks.insert(
        "unix_stream_splice_read".to_owned(),
        (vec![8, 8, 8, 8, 4], 5, 8),
    );
    hooks.insert("unix_stream_read_skb".to_owned(), (vec![8, 8], 2, 4));
    hooks.insert("unix_read_skb".to_owned(), (vec![8, 8], 2, 4));
    hooks.insert("unix_bpf_recvmsg".to_owned(), (vec![8, 8, 8, 4], 4, 4));
    // tcp_recvmsg_locked has no FUNC entry in this exact BTF. Its complete
    // installed image is required by grouped-target.h, not a fabricated type.
    ensure!(
        contract.btf_sha256 == legacy.btf_sha256 && contract.hooks == hooks,
        "grouped BTF or complete hook contract differs"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn value() -> Value {
        serde_json::from_slice(include_bytes!("accepted-grouped-v4-contract.json")).unwrap()
    }
    fn refuses(value: &Value) {
        assert!(Contract::parse(&serde_json::to_vec(value).unwrap()).is_err());
    }
    #[test]
    fn ftrace_topology_retains_role_coverage_without_group_runtime_sources() {
        let parsed=Contract::parse(include_bytes!("accepted-contract.json")).unwrap();
        assert!(parsed.ftrace_only);
        assert_eq!((parsed.maps,parsed.programs,parsed.links),(24,49,49));
        assert_eq!(parsed.maps+parsed.programs+parsed.links,122);
        let group=parsed.grouped_event.unwrap();
        assert_eq!((group.version,group.program.as_str(),group.cookie),
            (2,"ftrace-replacements-v1",0));
        assert_eq!(group.sites.len(),17);
        for legacy in ["grouped-io.c","grouped-owner.c","grouped-broker-bridge.c",
            "grouped-cleanup-bridge.c","grouped-adoption-wire.c"] {
            assert!(!parsed.source_files.iter().any(|source|source==legacy));
        }
    }
    #[test]
    fn grouped_topology_keeps_every_physical_site_under_fixed_inventory() {
        let parsed = Contract::parse(include_bytes!("accepted-grouped-v4-contract.json")).unwrap();
        assert_eq!((parsed.maps, parsed.programs, parsed.links), (23, 44, 44));
        assert_eq!(parsed.maps + parsed.programs + parsed.links, 111);
        assert!(parsed.maps + parsed.programs + parsed.links <= 128);
        let group = parsed.grouped_event.unwrap();
        assert_eq!(group.sites.len(), 17);
        assert_eq!(group.receive_entry.len(), 20);
        assert_eq!(group.receive_return.len(), 4);
        // Old copies' two linear and two fragment sites remain separate roles.
        assert_eq!(
            group.sites[11..15]
                .iter()
                .map(|s| (s.offset, s.cookie))
                .collect::<Vec<_>>(),
            vec![(0x64, 5), (0x69, 6), (0x26b, 7), (0x270, 8)]
        );
        assert_eq!(group.sites[15].symbol, "inet_recvmsg");
        assert_eq!(group.sites[16].symbol, "inet6_recvmsg");
    }
    #[test]
    fn grouped_missing_duplicate_reordered_or_mutated_sites_refuse() {
        for index in 0..17 {
            let mut v = value();
            v["grouped_event"]["sites"]
                .as_array_mut()
                .unwrap()
                .remove(index);
            refuses(&v);
            for key in ["role", "address", "offset", "cookie"] {
                let mut v = value();
                let old = v["grouped_event"]["sites"][index][key].as_u64().unwrap();
                v["grouped_event"]["sites"][index][key] = json!(old ^ 1);
                refuses(&v);
            }
            let mut v = value();
            v["grouped_event"]["sites"][index]["symbol"] = json!("other");
            refuses(&v);
            let mut v = value();
            v["grouped_event"]["sites"]
                .as_array_mut()
                .unwrap()
                .swap(index, (index + 1) % 17);
            refuses(&v);
            let mut v = value();
            let site = v["grouped_event"]["sites"][index].clone();
            v["grouped_event"]["sites"]
                .as_array_mut()
                .unwrap()
                .push(site);
            refuses(&v);
        }
        for label in ["receive_entry", "receive_return"] {
            let n = if label == "receive_entry" { 20 } else { 4 };
            for index in 0..n {
                let mut v = value();
                v["grouped_event"][label]
                    .as_array_mut()
                    .unwrap()
                    .remove(index);
                refuses(&v);
                for key in ["address", "cookie"] {
                    let mut v = value();
                    let old = v["grouped_event"][label][index][key].as_u64().unwrap();
                    v["grouped_event"][label][index][key] = json!(old ^ 1);
                    refuses(&v);
                }
                let mut v = value();
                v["grouped_event"][label][index]["symbol"] = json!("other");
                refuses(&v);
            }
        }
    }
    #[test]
    fn grouped_role_source_hook_and_format_downgrade_refuse() {
        let mut v = value();
        v.as_object_mut().unwrap().remove("grouped_event");
        refuses(&v);
        for (key, replacement) in [
            ("version", json!(0)),
            ("program", json!("other")),
            ("cookie", json!(0)),
            ("anchor_symbol", json!("other")),
            ("anchor_address", json!(0)),
        ] {
            let mut v = value();
            v["grouped_event"][key] = replacement;
            refuses(&v);
        }
        for (key, replacement) in [
            ("maps", json!(24)),
            ("programs", json!(43)),
            ("links", json!(45)),
            ("abi_version", json!("4150525553540008")),
            ("btf_sha256", json!("0".repeat(64))),
        ] {
            let mut v = value();
            v[key] = replacement;
            refuses(&v);
        }
        let base = value();
        for index in 0..base["source_files"].as_array().unwrap().len() {
            let mut v = value();
            v["source_files"].as_array_mut().unwrap().remove(index);
            refuses(&v);
        }
        for name in base["hooks"].as_object().unwrap().keys() {
            let mut v = value();
            v["hooks"].as_object_mut().unwrap().remove(name);
            refuses(&v);
            let mut v = value();
            v["hooks"][name][1] = json!(99);
            refuses(&v);
        }
        let mut v = value();
        v["grouped_event"]["unknown"] = json!(true);
        refuses(&v);
        let mut v = value();
        v["grouped_event"]["sites"][0]["unknown"] = json!(true);
        refuses(&v);
        let mut v = value();
        v["grouped_event"]["receive_entry"][0]["unknown"] = json!(true);
        refuses(&v);
    }

    fn current() -> Value {
        serde_json::from_slice(include_bytes!("accepted-contract.json")).unwrap()
    }

    #[test]
    fn ftrace_fault_witness_requires_exact_inventory_and_hook() {
        let selected = current();
        for (field, exact) in [("maps", 24), ("programs", 49), ("links", 49)] {
            for replacement in [exact - 1, exact + 1] {
                let mut changed = selected.clone();
                changed[field] = json!(replacement);
                refuses(&changed);
            }
        }
        let mut old_shape = selected.clone();
        old_shape["maps"] = json!(23);
        old_shape["programs"] = json!(47);
        old_shape["links"] = json!(47);
        refuses(&old_shape);
        let mut missing = selected.clone();
        missing["hooks"].as_object_mut().unwrap().remove("fixup_exception");
        refuses(&missing);
        for replacement in [json!([[8, 4, 8], 4, 4]), json!([[8, 4, 8, 8], 8, 4]), json!([[8, 4, 8, 8], 4, 3])] {
            let mut changed = selected.clone();
            changed["hooks"]["fixup_exception"] = replacement;
            refuses(&changed);
        }
        assert!(Contract::parse(&serde_json::to_vec(&selected).unwrap()).is_ok());
    }

    #[test]
    fn ftrace_contract_preserves_role_coverage_and_exact_source_population() {
        let historical = value();
        let selected = current();
        let parsed = Contract::parse(include_bytes!("accepted-contract.json")).unwrap();
        assert_eq!(parsed.accepted_copy_version().unwrap(), 5);
        assert_eq!(parsed.abi_version, "4150525553540008");
        assert!(parsed.ftrace_only);
        assert_eq!((parsed.maps, parsed.programs, parsed.links), (24, 49, 49));
        assert_eq!((historical["programs"].as_u64(),historical["links"].as_u64()),(Some(44),Some(44)));
        for field in ["schema", "btf_sha256", "shared_links"] {
            assert_eq!(selected[field], historical[field], "{field}");
        }
        assert_eq!(selected["maps"], json!(24));
        assert_eq!(historical["maps"], json!(23));
        let mut exact_hooks = historical["hooks"].clone();
        exact_hooks["fixup_exception"] = json!([[8, 4, 8, 8], 4, 4]);
        assert_eq!(selected["hooks"], exact_hooks);
        for field in ["anchor_symbol","anchor_address","sites","receive_entry","receive_return"] {
            assert_eq!(selected["grouped_event"][field],historical["grouped_event"][field],"{field}");
        }
        assert_eq!(selected["grouped_event"]["version"],json!(2));
        assert_eq!(selected["grouped_event"]["program"],json!("ftrace-replacements-v1"));
        assert_eq!(selected["grouped_event"]["cookie"],json!(0));
        let classic:Value=serde_json::from_slice(include_bytes!("accepted-classic-v40-contract.json")).unwrap();
        let classic_sources = classic["source_files"].as_array().unwrap();
        let selected_sources = selected["source_files"].as_array().unwrap();
        assert_eq!(classic_sources.len(), 23);
        assert_eq!(selected_sources.len(), 47);
        assert_eq!(&selected_sources[..23], classic_sources);
        assert_eq!(&selected_sources[23..], &[
            json!("ftrace-coverage.h"),json!("driver-grouped.c"),json!("grouped-driver.h"),
            json!("grouped-io.h"),json!("grouped-owner.h"),json!("grouped-probes.bpf.h"),
            json!("grouped-probes.h"),json!("grouped-target.h"),json!("provider-grouped.bpf.c"),
            json!("stream-membership.bpf.h"),json!("grouped_contract.rs"),
            json!("accepted-classic-v40-contract.json"),json!("accepted-grouped-v4-contract.json"),
            json!("fd-call-shared.bpf.h"),json!("fd-journal.bpf.h"),json!("stream-frontier.h"),
            json!("stream-copy-custody.inc"),
            json!("stream-copy-problem.inc"),
            json!("stream-copy-unit-enter.inc"),
            json!("stream-copy-emit.inc"),
            json!("stream-copy-unit-exit.inc"),
            json!("stream-copy-fault.h"),
            json!("stream-copy-fault.inc"),
            json!("fd-session-dispatch.inc"),
        ]);
        assert_eq!(parsed.source_files.len(), 47);
    }

    #[test]
    fn ftrace_contract_requires_both_exact_version_and_complete_ordered_sources() {
        let selected = current();
        for (abi, copy) in [
            ("4150525553540008", None),
            ("4150525553540008", Some(4)),
            ("4150525553540007", Some(5)),
            ("4150525553540009", Some(5)),
            ("4150525553540008", Some(6)),
            ("4150525553540007", Some(4)),
        ] {
            let mut changed = selected.clone();
            changed["abi_version"] = json!(abi);
            match copy {
                Some(v) => changed["copy_version"] = json!(v),
                None => { changed.as_object_mut().unwrap().remove("copy_version"); }
            }
            refuses(&changed);
        }
        let count=selected["source_files"].as_array().unwrap().len();
        assert_eq!(count,47);
        for index in 0..count {
            let mut changed = selected.clone();
            changed["source_files"].as_array_mut().unwrap().remove(index);
            refuses(&changed);
            let mut changed = selected.clone();
            changed["source_files"][index] = json!("other.h");
            refuses(&changed);
            let mut changed = selected.clone();
            changed["source_files"].as_array_mut().unwrap().swap(index, (index + 1) % count);
            refuses(&changed);
        }
        let mut extra = selected.clone();
        extra["source_files"].as_array_mut().unwrap().push(json!("extra.h"));
        refuses(&extra);
        let mut old_with_new_pair = value();
        old_with_new_pair["abi_version"] = json!("4150525553540008");
        old_with_new_pair["copy_version"] = json!(5);
        refuses(&old_with_new_pair);
        let mut explicit_old = value();
        explicit_old["copy_version"] = json!(4);
        assert_eq!(Contract::parse(&serde_json::to_vec(&explicit_old).unwrap()).unwrap().accepted_copy_version().unwrap(), 4);
    }
}
