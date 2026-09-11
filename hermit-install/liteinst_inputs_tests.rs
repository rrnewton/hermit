use std::process::Command;

use super::*;

#[test]
fn missing_source_record_is_watched_before_producer_creates_it() {
    let root = std::env::temp_dir().join(format!("installer-first-record-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let source = root.join("record.json");
    let mut environment = BTreeMap::new();
    environment.insert(
        "HERMIT_LITEINST_SOURCE_RECORD".into(),
        source.clone().into_os_string(),
    );
    let expected = format!("cargo:rerun-if-changed={}", source.display());
    assert!(directives(&root, &environment).unwrap().contains(&expected));
    fs::write(&source, b"not-json").unwrap();
    assert!(directives(&root, &environment).is_err());
    fs::write(
        &source,
        br#"{"hermit_files":{"new.rs":{}},"reverie_files":{}}"#,
    )
    .unwrap();
    environment.insert(
        "HERMIT_LITEINST_HERMIT_ROOT".into(),
        root.clone().into_os_string(),
    );
    environment.insert(
        "HERMIT_LITEINST_REVERIE_ROOT".into(),
        root.clone().into_os_string(),
    );
    assert!(directives(&root, &environment).unwrap().contains(&format!(
        "cargo:rerun-if-changed={}",
        root.join("new.rs").display()
    )));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn incremental_installer_rechecks_same_path_identity_and_inputs() {
    let directory =
        std::env::temp_dir().join(format!("liteinst-installer-inputs-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let repository = directory.join("repository");
    let reverie = directory.join("dependency");
    let package = directory.join("probe");
    fs::create_dir_all(&package).unwrap();
    fs::create_dir_all(&reverie).unwrap();
    for directive in directives(&repository, &BTreeMap::new()).unwrap() {
        if let Some(path) = directive.strip_prefix("cargo:rerun-if-changed=") {
            let path = Path::new(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            if path.file_name().unwrap() == "src" {
                fs::create_dir(path).unwrap();
            } else {
                fs::write(path, "input").unwrap();
            }
        }
    }
    let record = directory.join("identity.json");
    let config = directory.join("config.toml");
    let consumer = repository.join("liteinst-runtime-build/detcore-runtime/src/lib.rs");
    fs::write(&consumer, "first consumer").unwrap();
    fs::write(reverie.join("input.rs"), "first dependency").unwrap();
    fs::write(&config, "first config").unwrap();
    let identity = serde_json::json!({"hermit_files": {"liteinst-runtime-build/detcore-runtime/src/lib.rs": "first"}, "reverie_files": {"input.rs": "first"}, "revision": 1});
    fs::write(&record, serde_json::to_vec(&identity).unwrap()).unwrap();
    fs::write(package.join("Cargo.toml"), "[package]\nname='installer-input-probe'\nversion='0.0.0'\nedition='2024'\n[workspace]\n[lib]\npath='lib.rs'\n[build-dependencies]\nserde_json='1.0.151'\n").unwrap();
    fs::write(package.join("lib.rs"), "").unwrap();
    let helper = Path::new(file!())
        .parent()
        .unwrap()
        .join("liteinst_inputs.rs")
        .canonicalize()
        .unwrap();
    fs::write(package.join("build.rs"), format!(r#"
#[path = {helper:?}]
mod inputs;
fn main() {{
    println!("cargo:rerun-if-changed=build.rs");
    let root = std::path::PathBuf::from(std::env::var_os("HERMIT_LITEINST_HERMIT_ROOT").unwrap());
    inputs::emit(&root).unwrap();
    inputs::require_normal_mode().unwrap();
    let bytes = std::fs::read(std::env::var_os("HERMIT_LITEINST_SOURCE_RECORD").unwrap()).unwrap();
    let _: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    use std::io::Write;
    writeln!(std::fs::OpenOptions::new().create(true).append(true).open(root.join("staged")).unwrap(), "{{}}", String::from_utf8(bytes).unwrap()).unwrap();
}}
"#)).unwrap();
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command
        .args(["build", "--offline", "--manifest-path"])
        .arg(package.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(directory.join("target"))
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_BUILD_JOBS", "1")
        .env("HERMIT_LITEINST_SOURCE_RECORD", &record)
        .env("HERMIT_LITEINST_HERMIT_ROOT", &repository)
        .env("HERMIT_LITEINST_REVERIE_ROOT", &reverie)
        .env("HERMIT_LITEINST_CARGO_CONFIG", &config)
        .env("HERMIT_LITEINST_DIAGNOSTIC", "0");
    for name in [
        "HERMIT_LITEINST_CLI_MANIFEST",
        "HERMIT_LITEINST_DSO_MANIFEST",
        "HERMIT_LITEINST_BUILD_MANIFEST",
    ] {
        command.env_remove(name);
    }
    let staged = repository.join("staged");
    let mut attempt = 0;
    let mut run = |command: &mut Command, expected: usize, success: bool| {
        let output = command.output().unwrap();
        attempt += 1;
        fs::write(
            directory.join(format!("attempt-{attempt}.stdout")),
            &output.stdout,
        )
        .unwrap();
        fs::write(
            directory.join(format!("attempt-{attempt}.stderr")),
            &output.stderr,
        )
        .unwrap();
        assert_eq!(
            output.status.success(),
            success,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(&staged).unwrap().lines().count(),
            expected
        );
        std::thread::sleep(std::time::Duration::from_millis(1100));
    };
    run(&mut command, 1, true);
    run(&mut command, 1, true);
    let mut changed = identity;
    changed["revision"] = serde_json::json!(2);
    fs::write(&record, serde_json::to_vec(&changed).unwrap()).unwrap();
    run(&mut command, 2, true);
    assert!(
        fs::read_to_string(&staged)
            .unwrap()
            .lines()
            .last()
            .unwrap()
            .contains("\"revision\":2")
    );
    fs::write(&config, "second config").unwrap();
    run(&mut command, 3, true);
    fs::write(&consumer, "second consumer").unwrap();
    run(&mut command, 4, true);
    fs::write(reverie.join("input.rs"), "second dependency").unwrap();
    run(&mut command, 5, true);
    let alternate = directory.join("other-identity.json");
    fs::copy(&record, &alternate).unwrap();
    command.env("HERMIT_LITEINST_SOURCE_RECORD", alternate);
    run(&mut command, 6, true);
    command.env("HERMIT_LITEINST_DIAGNOSTIC", "1");
    run(&mut command, 6, false);
    assert!(
        fs::read_to_string(directory.join("attempt-8.stderr"))
            .unwrap()
            .contains("diagnostic Detcore artifacts cannot be installed")
    );
    eprintln!(
        "incremental installer stub evidence: {}",
        directory.display()
    );
}

#[test]
fn watches_cover_selected_manifests_and_configuration() {
    let repository = Path::new("/source");
    let mut environment = BTreeMap::new();
    for name in [
        "HERMIT_LITEINST_CLI_MANIFEST",
        "HERMIT_LITEINST_DSO_MANIFEST",
        "HERMIT_LITEINST_BUILD_MANIFEST",
    ] {
        environment.insert(
            name.to_owned(),
            OsString::from(format!("/overrides/{name}/Cargo.toml")),
        );
    }
    environment.insert(
        "HERMIT_LITEINST_CARGO_CONFIG".into(),
        "/overrides/config.toml".into(),
    );
    let lines = directives(repository, &environment).unwrap();
    for name in INPUTS {
        assert!(lines.contains(&format!("cargo:rerun-if-env-changed={name}")));
    }
    for (name, value) in environment {
        assert!(lines.contains(&format!(
            "cargo:rerun-if-changed={}",
            Path::new(&value).display()
        )));
        if name.ends_with("MANIFEST") {
            for file in ["Cargo.lock", "src"] {
                assert!(lines.contains(&format!(
                    "cargo:rerun-if-changed={}",
                    Path::new(&value).parent().unwrap().join(file).display()
                )));
            }
        }
    }
}
#[test]
fn private_kind_has_distinct_resource_and_rejects_unknown_kind() {
    use std::ffi::OsStr;
    assert_eq!(
        super::runtime_name(Some(OsStr::new("private-crt"))).unwrap(),
        "hermit_liteinst_detcore_private.elf"
    );
    assert_eq!(
        super::runtime_name(None).unwrap(),
        "libhermit_liteinst_detcore.so"
    );
    assert!(super::runtime_name(Some(OsStr::new("private"))).is_err());
}
