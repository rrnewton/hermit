// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn generated_export_isolates_state_without_authorizing_nested_execution() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let scratch = std::env::temp_dir().join(format!(
        "hermit-export-isolation-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&scratch).unwrap();
    let plan = scratch.join("plan.json");
    // This live test process is an actual ancestor of both validator children.
    // Only the generator may discard its marker for a private metadata export.
    let active = std::process::id().to_string();
    let host_fact = |program, argument| {
        let output = Command::new(program).arg(argument).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let machine = host_fact("hostname", "-s");
    let kernel = host_fact("uname", "-r");
    let output = Command::new(env!("CARGO_BIN_EXE_generate-validation-dag"))
        .current_dir(root)
        .arg("--write")
        .arg(&plan)
        .env("TMPDIR", &scratch)
        .env("HERMIT_VALIDATE_ACTIVE", &active)
        .env("E2E_MACHINE_SHORTNAME", &machine)
        .env("E2E_KERNEL_VERSION", &kernel)
        .env_remove("VALIDATE_RUN_STATE")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let generated = fs::read(&plan).unwrap();
    assert_eq!(
        generated,
        fs::read(root.join("ci/dag/validate.json")).unwrap()
    );
    assert_eq!(fs::read_dir(&scratch).unwrap().count(), 1);

    // Direct nested exports still require the outer state. Supplying the real
    // forwarded host facts ensures this reaches that guard, not an earlier one.
    let direct_plan = scratch.join("must-not-exist.json");
    let missing_state = Command::new(root.join("scripts/validate.rs"))
        .current_dir(root)
        .arg("--write-generated-plan")
        .arg(&direct_plan)
        .env("HERMIT_VALIDATE_ACTIVE", &active)
        .env("E2E_MACHINE_SHORTNAME", &machine)
        .env("E2E_KERNEL_VERSION", &kernel)
        .env_remove("VALIDATE_RUN_STATE")
        .output()
        .unwrap();
    assert_eq!(missing_state.status.code(), Some(75), "{missing_state:?}");
    let stdout = String::from_utf8(missing_state.stdout).unwrap();
    assert!(stdout.contains("refused by: run-state setup"), "{stdout}");
    assert!(
        stdout.contains("nested validation did not inherit VALIDATE_RUN_STATE from its outer run"),
        "{stdout}"
    );
    assert!(stdout.contains("nodes: none executed"), "{stdout}");
    assert!(!direct_plan.exists());

    let refused = Command::new(root.join("scripts/validate.rs"))
        .current_dir(root)
        .arg("full")
        .env("HERMIT_VALIDATE_ACTIVE", &active)
        .env_remove("VALIDATE_RUN_STATE")
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(75), "{refused:?}");
    let stdout = String::from_utf8(refused.stdout).unwrap();
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(stdout.contains("the re-entrancy guard"), "{stdout}");
    assert!(stdout.contains("nodes: none executed"), "{stdout}");
    assert!(
        stdout.contains("FINAL_VALIDATE_STATUS: COULD_NOT_RUN"),
        "{stdout}"
    );
    assert!(
        stderr.contains("nested invocations may only run a focused mode"),
        "{stderr}"
    );
    assert_eq!(fs::read(&plan).unwrap(), generated);
    assert_eq!(fs::read_dir(&scratch).unwrap().count(), 1);
    fs::remove_dir_all(scratch).unwrap();
}
