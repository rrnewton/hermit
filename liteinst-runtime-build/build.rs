use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

mod artifact;
#[path = "../hermit-cli/src/liteinst_artifact.rs"]
pub mod liteinst_artifact;
mod private_build;

fn path(name: &str) -> PathBuf {
    PathBuf::from(env::var_os(name).unwrap_or_else(|| panic!("{name} is required")))
}

fn cargo(subcommand: &str, config: &Option<PathBuf>) -> Command {
    let mut command = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command.arg(subcommand);
    if let Some(config) = config {
        command.arg("--config").arg(config);
    }
    command
        .env_remove("HERMIT_LITEINST_STAGE")
        .env("CARGO_NET_OFFLINE", "true");
    sanitize(&mut command);
    command
}

pub(crate) fn sanitize(command: &mut Command) {
    let explicitly_bound: BTreeSet<_> = command
        .get_envs()
        .filter_map(|(key, value)| value.map(|_| key.to_os_string()))
        .collect();
    for (key, _) in env::vars_os() {
        let key_text = key.to_string_lossy();
        if !explicitly_bound.contains(&key)
            && (key_text.starts_with("LD_")
                || key_text.starts_with("DYLD_")
                || key_text.starts_with("CARGO_BUILD_")
                || key_text.starts_with("CARGO_PROFILE_")
                || key_text.starts_with("CARGO_TARGET_")
                || key_text.starts_with("CC_")
                || key_text.starts_with("CXX_")
                || key_text.starts_with("CFLAGS_")
                || key_text.starts_with("CXXFLAGS_")
                || key_text.starts_with("AR_")
                || key_text.starts_with("RANLIB_")
                || key_text.starts_with("PKG_CONFIG_")
                || key_text.ends_with("_CC")
                || key_text.ends_with("_CXX")
                || key_text.ends_with("_CFLAGS")
                || key_text.ends_with("_CXXFLAGS")
                || key_text.ends_with("_AR")
                || key_text.ends_with("_RANLIB")
                || matches!(
                    key_text.as_ref(),
                    "AR" | "CC"
                        | "CFLAGS"
                        | "CXX"
                        | "CXXFLAGS"
                        | "COMPILER_PATH"
                        | "CPATH"
                        | "C_INCLUDE_PATH"
                        | "CPLUS_INCLUDE_PATH"
                        | "GCC_EXEC_PREFIX"
                        | "LIBRARY_PATH"
                        | "RANLIB"
                        | "RUSTC"
                        | "RUSTDOC"
                        | "RUSTC_WRAPPER"
                        | "RUSTC_WORKSPACE_WRAPPER"
                        | "RUSTC_BOOTSTRAP"
                        | "RUSTFLAGS"
                        | "RUSTDOCFLAGS"
                        | "CARGO_ENCODED_RUSTFLAGS"
                        | "RUSTUP_TOOLCHAIN"
                        | "RUSTUP_OVERRIDE_HOST_TRIPLE"
                        | "LDFLAGS"
                        | "CPP"
                        | "CPPFLAGS"
                        | "AS"
                        | "LD"
                        | "NM"
                        | "OBJCOPY"
                        | "OBJDUMP"
                        | "STRIP"
                        | "PKG_CONFIG"
                        | "QEMU_LD_PREFIX"
                        | "GLIBC_TUNABLES"
                        | "BASH_ENV"
                        | "ENV"
                ))
        {
            command.env_remove(key);
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=HERMIT_LITEINST_STAGE");
    if env::var_os("HERMIT_LITEINST_STAGE").is_none() {
        return;
    }
    for name in [
        "HERMIT_LITEINST_STAGE",
        "HERMIT_LITEINST_SOURCE_RECORD",
        "HERMIT_LITEINST_HERMIT_ROOT",
        "HERMIT_LITEINST_REVERIE_ROOT",
        "HERMIT_LITEINST_CLI_MANIFEST",
        "HERMIT_LITEINST_DSO_MANIFEST",
        "HERMIT_LITEINST_CARGO_CONFIG",
        "HERMIT_LITEINST_DIAGNOSTIC",
        "HERMIT_LITEINST_RUNTIME_KIND",
        "HERMIT_LITEINST_PRIVATE_INPUTS",
        "PROFILE",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    for name in [
        "build.rs",
        "artifact.rs",
        "private_build.rs",
        "private-native",
        "Cargo.toml",
        "Cargo.lock",
        "../hermit-cli/src/liteinst_artifact.rs",
        "../hermit-cli/src/liteinst_artifact_private.rs",
    ] {
        println!("cargo:rerun-if-changed={name}");
    }
    let hermit = path("HERMIT_LITEINST_HERMIT_ROOT").canonicalize().unwrap();
    let reverie = path("HERMIT_LITEINST_REVERIE_ROOT").canonicalize().unwrap();
    let cli = env::var_os("HERMIT_LITEINST_CLI_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|| hermit.join("hermit-cli/Cargo.toml"));
    let dso = env::var_os("HERMIT_LITEINST_DSO_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|| hermit.join("liteinst-runtime-build/detcore-runtime/Cargo.toml"));
    let config = env::var_os("HERMIT_LITEINST_CARGO_CONFIG").map(PathBuf::from);
    let source = path("HERMIT_LITEINST_SOURCE_RECORD");
    let destination = path("HERMIT_LITEINST_STAGE");
    let diagnostic = env::var("HERMIT_LITEINST_DIAGNOSTIC").as_deref() == Ok("1");
    if diagnostic {
        let parent = destination.parent().unwrap().canonicalize().unwrap();
        assert!(
            !parent.starts_with(&hermit) && !parent.starts_with(&reverie),
            "diagnostic staging must be outside product trees"
        );
    }
    let mut pin_command = Command::new(hermit.join("ci/run-reverie-pin-check.sh"));
    pin_command.args(["--repo"]).arg(&hermit).arg("--print-pin");
    sanitize(&mut pin_command);
    let pin_output = pin_command.output().unwrap();
    assert!(
        pin_output.status.success(),
        "pin check failed: {}",
        String::from_utf8_lossy(&pin_output.stderr)
    );
    let pin = String::from_utf8(pin_output.stdout).unwrap();
    let inputs = liteinst_artifact::SourceInputs {
        hermit: &hermit,
        reverie: &reverie,
        cli_manifest: &cli,
        dso_manifest: &dso,
        config: config.as_deref(),
        evidence: source.parent().unwrap(),
        pin: pin.trim(),
        diagnostic,
    };
    if !source.exists() {
        use std::io::Write;
        let bytes = serde_json::to_vec_pretty(
            &liteinst_artifact::source_record(&inputs)
                .expect("capture actual source/dependency record"),
        )
        .unwrap();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&source)
            .expect("create source record");
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
    }
    let private = liteinst_artifact::private::requested().expect("runtime build kind");
    if private {
        assert!(
            env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64")
                && env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux"),
            "private runtime requires x86-64 Linux"
        );
        println!(
            "cargo:rerun-if-changed={}",
            path("HERMIT_LITEINST_PRIVATE_INPUTS").display()
        );
    }
    let (identity, record) = liteinst_artifact::verify_source_record(&source, &inputs)
        .expect("verify source/dependency record before build");
    println!("cargo:rerun-if-changed={}", source.display());
    for (role, root) in [("hermit_files", &hermit), ("reverie_files", &reverie)] {
        for file in record[role].as_object().unwrap().keys() {
            println!("cargo:rerun-if-changed={}", root.join(file).display());
        }
    }
    for file in [&cli, &dso].into_iter().chain(config.iter()) {
        println!("cargo:rerun-if-changed={}", file.display());
    }
    let mut metadata_command = cargo("metadata", &config);
    if private {
        metadata_command.args(["--no-default-features", "--features", "private-crt"]);
    }
    let metadata = metadata_command
        .args([
            "--offline",
            "--locked",
            "--format-version=1",
            "--manifest-path",
        ])
        .arg(&dso)
        .output()
        .unwrap();
    assert!(metadata.status.success(), "runtime metadata failed");
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout).unwrap();
    let package = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| {
            package["name"] == "hermit-liteinst-detcore-runtime"
                && PathBuf::from(package["manifest_path"].as_str().unwrap())
                    .canonicalize()
                    .unwrap()
                    == dso.canonicalize().unwrap()
        })
        .expect("actual Detcore runtime package");
    let package_id = package["id"].as_str().unwrap();
    let profile = match env::var("PROFILE").unwrap().as_str() {
        "debug" => "dev",
        "release" => "release",
        other => panic!("unsupported profile {other}"),
    };
    let artifact = if private {
        private_build::build(
            &hermit,
            &dso,
            &config,
            package_id,
            profile,
            &path("OUT_DIR").join(format!(
                "private-runtime-{}",
                &liteinst_artifact::digest(destination.as_os_str().as_encoded_bytes())[..16]
            )),
        )
        .expect("genuine private runtime build")
    } else {
        let output = cargo("build", &config)
            .args(["--offline", "--locked", "--manifest-path"])
            .arg(&dso)
            .args([
                "-p",
                "hermit-liteinst-detcore-runtime",
                "--profile",
                profile,
                "--target-dir",
            ])
            .arg(path("OUT_DIR").join("runtime-target"))
            .arg("--message-format=json-render-diagnostics")
            .output()
            .unwrap();
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(
            output.status.success(),
            "actual Detcore runtime build failed"
        );
        artifact::selected_cdylib(std::str::from_utf8(&output.stdout).unwrap(), package_id).unwrap()
    };
    let bytes = fs::read(&artifact).unwrap();
    if private {
        liteinst_artifact::validate_private_runtime(&bytes)
    } else {
        liteinst_artifact::validate_constructor(&bytes)
    }
    .expect("actual runtime constructor validation");
    let (after, _) = liteinst_artifact::verify_source_record(&source, &inputs)
        .expect("verify sources after build");
    assert_eq!(identity, after, "source record changed during build");
    let provenance =
        serde_json::to_vec_pretty(&liteinst_artifact::provenance(&bytes, &identity, &record))
            .unwrap();
    liteinst_artifact::stage_pair(
        &destination,
        &bytes,
        &provenance,
        pin.trim(),
        &identity,
        diagnostic,
    )
    .expect("protected Detcore staging");
    println!(
        "cargo:warning=staged {} ({})",
        if private {
            liteinst_artifact::private::RUNTIME_NAME
        } else {
            liteinst_artifact::RUNTIME_NAME
        },
        liteinst_artifact::digest(&bytes)
    );
}
