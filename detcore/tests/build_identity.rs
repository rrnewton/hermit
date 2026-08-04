/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

#[test]
fn locked_host_and_helpers_resolve_one_detcore_graph() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("Detcore manifest has no workspace parent");
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--locked"])
        .arg("--manifest-path")
        .arg(workspace.join("Cargo.toml"))
        .output()
        .expect("run locked workspace metadata");
    assert!(
        output.status.success(),
        "locked metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
    let package_ids = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|package| {
            (
                package["name"].as_str().unwrap(),
                package["id"].as_str().unwrap(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let detcore = package_ids["detcore"];
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| (node["id"].as_str().unwrap(), node))
        .collect::<BTreeMap<_, _>>();

    for consumer in [
        "hermit",
        "hermit-dynamorio",
        "hermit-sabre",
        "hermit-e9patch",
    ] {
        let resolved = nodes[package_ids[consumer]]["deps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|dependency| dependency["name"].as_str() == Some("detcore"))
            .map(|dependency| dependency["pkg"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(
            !resolved.is_empty(),
            "{consumer} has no locked Detcore dependency"
        );
        assert!(
            resolved.iter().all(|package| *package == detcore),
            "{consumer} resolved a different Detcore package: {resolved:?}"
        );
    }

    assert_eq!(nodes[detcore]["features"], serde_json::json!([]));
}
