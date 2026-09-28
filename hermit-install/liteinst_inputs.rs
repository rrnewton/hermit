use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const INPUTS: &[&str] = &[
    "HERMIT_LITEINST_SOURCE_RECORD",
    "HERMIT_LITEINST_HERMIT_ROOT",
    "HERMIT_LITEINST_REVERIE_ROOT",
    "HERMIT_LITEINST_CLI_MANIFEST",
    "HERMIT_LITEINST_DSO_MANIFEST",
    "HERMIT_LITEINST_BUILD_MANIFEST",
    "HERMIT_LITEINST_CARGO_CONFIG",
    "HERMIT_LITEINST_DIAGNOSTIC",
    "HERMIT_LITEINST_RUNTIME_KIND",
    "HERMIT_LITEINST_PRIVATE_INPUTS",
];

pub fn directives(
    repository: &Path,
    environment: &BTreeMap<String, OsString>,
) -> io::Result<Vec<String>> {
    let mut output: Vec<_> = INPUTS
        .iter()
        .map(|name| format!("cargo:rerun-if-env-changed={name}"))
        .collect();
    let mut files: Vec<PathBuf> = [
        "hermit-install/liteinst_inputs.rs",
        "scripts/stage-liteinst-runtime.sh",
        "liteinst-runtime-build/build.rs",
        "liteinst-runtime-build/artifact.rs",
        "liteinst-runtime-build/private_build.rs",
        "liteinst-runtime-build/private-native",
        "liteinst-runtime-build/legacy.rs",
        "liteinst-runtime-build/legacy_artifact.rs",
        "liteinst-runtime-build/Cargo.toml",
        "liteinst-runtime-build/Cargo.lock",
        "liteinst-runtime-build/detcore-runtime/Cargo.toml",
        "liteinst-runtime-build/detcore-runtime/Cargo.lock",
        "liteinst-runtime-build/detcore-runtime/src",
        "hermit-cli/src/liteinst_artifact.rs",
        "hermit-cli/src/liteinst_artifact_private.rs",
        "detcore/Cargo.toml",
    ]
    .iter()
    .map(|file| repository.join(file))
    .collect();
    for name in [
        "HERMIT_LITEINST_CLI_MANIFEST",
        "HERMIT_LITEINST_DSO_MANIFEST",
        "HERMIT_LITEINST_BUILD_MANIFEST",
    ] {
        if let Some(path) = environment.get(name).map(PathBuf::from) {
            if let Some(parent) = path.parent() {
                files.push(parent.join("Cargo.lock"));
                files.push(parent.join("src"));
            }
            files.push(path);
        }
    }
    if let Some(config) = environment.get("HERMIT_LITEINST_CARGO_CONFIG") {
        files.push(PathBuf::from(config));
    }
    if let Some(path) = environment.get("HERMIT_LITEINST_PRIVATE_INPUTS") {
        let path = PathBuf::from(path);
        let record: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        files.push(path);
        for entry in record["files"]
            .as_object()
            .ok_or_else(|| io::Error::other("native file roles missing"))?
            .values()
        {
            files.push(PathBuf::from(
                entry["path"]
                    .as_str()
                    .ok_or_else(|| io::Error::other("native file path missing"))?,
            ));
        }
        for entry in record["headers"]
            .as_object()
            .ok_or_else(|| io::Error::other("native header roles missing"))?
            .values()
        {
            files.push(PathBuf::from(
                entry["path"]
                    .as_str()
                    .ok_or_else(|| io::Error::other("native header path missing"))?,
            ));
        }
    }
    if let Some(source) = environment.get("HERMIT_LITEINST_SOURCE_RECORD") {
        let source = PathBuf::from(source);
        let record = match fs::read(&source) {
            Ok(bytes) => Some(serde_json::from_slice::<serde_json::Value>(&bytes)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        files.push(source);
        if let Some(record) = record {
            for (role, variable) in [
                ("hermit_files", "HERMIT_LITEINST_HERMIT_ROOT"),
                ("reverie_files", "HERMIT_LITEINST_REVERIE_ROOT"),
            ] {
                let root = environment
                    .get(variable)
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, format!("missing {variable}"))
                    })?;
                let entries = record[role].as_object().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("missing {role}"))
                })?;
                for path in entries.keys() {
                    if !Path::new(path)
                        .components()
                        .all(|part| matches!(part, std::path::Component::Normal(_)))
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "non-relative source entry",
                        ));
                    }
                    files.push(root.join(path));
                }
            }
        }
    }
    files.sort();
    files.dedup();
    output.extend(
        files
            .into_iter()
            .map(|file| format!("cargo:rerun-if-changed={}", file.display())),
    );
    Ok(output)
}

pub fn emit(repository: &Path) -> io::Result<()> {
    let environment = INPUTS
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| ((*name).to_owned(), value)))
        .collect();
    for directive in directives(repository, &environment)? {
        println!("{directive}");
    }
    Ok(())
}

pub fn require_normal_mode() -> io::Result<()> {
    if std::env::var("HERMIT_LITEINST_DIAGNOSTIC").as_deref() == Ok("1") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "diagnostic Detcore artifacts cannot be installed into normal resources",
        ));
    }
    Ok(())
}

pub fn runtime_name(kind: Option<&std::ffi::OsStr>) -> io::Result<&'static str> {
    match kind.and_then(std::ffi::OsStr::to_str) {
        None if kind.is_none() => Ok("libhermit_liteinst_detcore.so"),
        Some("preload") => Ok("libhermit_liteinst_detcore.so"),
        Some("private-crt") => Ok("hermit_liteinst_detcore_private.elf"),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported LiteInst runtime build kind",
        )),
    }
}

#[cfg(test)]
#[path = "liteinst_inputs_tests.rs"]
mod tests;
