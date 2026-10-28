/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Shared parsing for the source-owned top-level compatibility producer and
//! Cargo build script. Neither the core checkout's lock nor a declared SHA is
//! accepted in place of the actual workspace's metadata and artifact units.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

pub const INPUT_ENV: &str = "HERMIT_COMPATIBILITY_BUILD_INPUT";
pub const INPUT_SCHEMA: &str = "hermit-compatibility-build-input/v2";
const MAX_INPUT_BYTES: usize = 16 * 1024;
const MAX_GRAPH_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputFile {
    pub path: PathBuf,
    pub sha256: String,
}

impl InputFile {
    pub fn read(&self, limit: usize) -> Result<Vec<u8>, String> {
        use std::io::Read;
        let file = std::fs::File::open(&self.path)
            .map_err(|error| format!("cannot open {}: {error}", self.path.display()))?;
        let mut bytes = Vec::new();
        file.take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read {}: {error}", self.path.display()))?;
        if bytes.len() > limit || format!("{:x}", Sha256::digest(&bytes)) != self.sha256 {
            return Err(format!(
                "compatibility producer input changed or exceeds its bound: {}",
                self.path.display()
            ));
        }
        Ok(bytes)
    }
}

/// File identities describe actual command outputs, not a caller-asserted
/// source revision. The build script reparses them and measures source roots.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildInput {
    pub schema: String,
    pub metadata: InputFile,
    pub first_artifacts: InputFile,
    pub artifacts: InputFile,
    pub rustc_first: InputFile,
    pub rustc_second: InputFile,
    pub source_snapshot: InputFile,
    pub manifest: InputFile,
    pub lock: InputFile,
    pub package: String,
    pub binary: String,
    pub target: String,
    pub profile: String,
}

impl BuildInput {
    pub fn load(path: &Path) -> Result<Self, String> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|error| error.to_string())?
            .take((MAX_INPUT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() > MAX_INPUT_BYTES {
            return Err("compatibility build input is too large".to_owned());
        }
        let input: Self = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if input.schema != INPUT_SCHEMA
            || input.package.is_empty()
            || input.binary.is_empty()
            || input.target.is_empty()
            || input.profile.is_empty()
        {
            return Err("unsupported or incomplete compatibility build input".to_owned());
        }
        Ok(input)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub version: String,
    pub source: Option<String>,
    pub manifest_path: PathBuf,
    pub targets: Vec<Target>,
}

#[derive(Debug, Deserialize)]
struct DepKind {
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Dependency {
    pkg: String,
    dep_kinds: Vec<DepKind>,
}

#[derive(Debug, Deserialize)]
struct Node {
    id: String,
    deps: Vec<Dependency>,
}

#[derive(Debug, Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    version: u64,
    packages: Vec<Package>,
    resolve: Option<Resolve>,
    workspace_root: PathBuf,
}

/// A real Cargo unit, including its features and Cargo's partial artifact
/// profile. Actual shared-unit rustc captures supply complete explicit options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unit {
    pub package: String,
    pub version: String,
    pub target: String,
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
    pub features: Vec<String>,
    pub profile: serde_json::Value,
}

impl Unit {
    pub fn key(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Target {
    pub name: String,
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
    pub src_path: PathBuf,
}

/// Raw Cargo locators are retained; only an actual matching compiler source
/// resolves these paths. Unbuilt metadata targets need not exist on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocators {
    pub declared: PathBuf,
    pub emitted: PathBuf,
}

pub fn canonical_source_file(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("actual Cargo source locator is not absolute".to_owned());
    }
    let resolved = path.canonicalize().map_err(|error| {
        format!(
            "cannot resolve actual Cargo source {}: {error}",
            path.display()
        )
    })?;
    if !resolved.is_file() {
        return Err("actual Cargo source is not a regular file".to_owned());
    }
    Ok(resolved)
}

fn metadata_packages(packages: Vec<Package>) -> Result<BTreeMap<String, Package>, String> {
    let mut result = BTreeMap::new();
    let mut manifests = BTreeSet::new();
    for mut package in packages {
        package.manifest_path = canonical_source_file(&package.manifest_path)?;
        if package
            .manifest_path
            .file_name()
            .is_none_or(|name| name != "Cargo.toml")
            || !manifests.insert(package.manifest_path.clone())
        {
            return Err("actual metadata has duplicate or noncanonical manifest owners".to_owned());
        }
        if result.insert(package.id.clone(), package).is_some() {
            return Err("actual metadata repeats a package ID".to_owned());
        }
    }
    Ok(result)
}

#[derive(Debug, Deserialize)]
struct Artifact {
    package_id: String,
    target: Target,
    profile: serde_json::Value,
    features: Vec<String>,
    executable: Option<PathBuf>,
    filenames: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct Graph {
    pub packages: BTreeMap<String, Package>,
    edges: BTreeMap<String, BTreeSet<String>>,
    pub units: BTreeMap<String, Vec<Unit>>,
    pub executables: BTreeMap<(String, String), PathBuf>,
    pub artifacts: BTreeMap<PathBuf, (String, Unit)>,
    pub source_locators: BTreeMap<(String, String), SourceLocators>,
    pub build_outputs: BTreeMap<String, BTreeSet<PathBuf>>,
}

impl Graph {
    pub fn read(input: &BuildInput) -> Result<Self, String> {
        input.manifest.read(MAX_GRAPH_BYTES)?;
        input.lock.read(MAX_GRAPH_BYTES)?;
        let metadata: Metadata = serde_json::from_slice(&input.metadata.read(MAX_GRAPH_BYTES)?)
            .map_err(|error| format!("invalid actual Cargo metadata: {error}"))?;
        if metadata.version != 1 {
            return Err("unsupported Cargo metadata version".to_owned());
        }
        let root = metadata
            .workspace_root
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if input.lock.path != root.join("Cargo.lock")
            || input.manifest.path != root.join("Cargo.toml")
        {
            return Err(
                "compatibility manifest/lock are not the actual top-level metadata workspace"
                    .to_owned(),
            );
        }
        let packages = metadata_packages(metadata.packages)?;
        let mut edges = BTreeMap::new();
        for node in metadata
            .resolve
            .ok_or_else(|| "Cargo metadata has no resolved graph".to_owned())?
            .nodes
        {
            let mut normal = BTreeSet::new();
            for dependency in node.deps {
                if dependency
                    .dep_kinds
                    .iter()
                    .any(|kind| kind.kind.is_none() || kind.kind.as_deref() == Some("build"))
                {
                    normal.insert(dependency.pkg);
                }
            }
            if edges.insert(node.id, normal).is_some() {
                return Err("actual metadata repeats a resolve node".to_owned());
            }
        }
        let mut graph = Self {
            packages,
            edges,
            units: BTreeMap::new(),
            executables: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            source_locators: BTreeMap::new(),
            build_outputs: BTreeMap::new(),
        };
        graph.read_artifacts(&input.artifacts.read(MAX_GRAPH_BYTES)?)?;
        Ok(graph)
    }

    pub fn read_artifacts(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut units: BTreeMap<String, BTreeMap<String, Unit>> = BTreeMap::new();
        let mut executables = BTreeMap::new();
        let mut artifacts = BTreeMap::new();
        let mut source_locators = BTreeMap::new();
        let mut build_outputs: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
        let mut finished = false;
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let value: serde_json::Value = serde_json::from_slice(line)
                .map_err(|error| format!("invalid Cargo artifact stream: {error}"))?;
            match value.get("reason").and_then(serde_json::Value::as_str) {
                Some("build-finished") => {
                    if finished
                        || value.get("success").and_then(serde_json::Value::as_bool) != Some(true)
                    {
                        return Err(
                            "Cargo artifact build did not finish successfully exactly once"
                                .to_owned(),
                        );
                    }
                    finished = true;
                }
                Some("compiler-artifact") => {
                    if finished {
                        return Err("Cargo emitted an artifact after build-finished".to_owned());
                    }
                    let artifact: Artifact =
                        serde_json::from_value(value).map_err(|error| error.to_string())?;
                    let package = self.packages.get(&artifact.package_id).ok_or_else(|| {
                        "built package is absent from the actual metadata".to_owned()
                    })?;
                    let declared: Vec<_> = package
                        .targets
                        .iter()
                        .filter(|target| {
                            target.name == artifact.target.name
                                && target.kind == artifact.target.kind
                                && target.crate_types == artifact.target.crate_types
                        })
                        .collect();
                    if declared.len() != 1 {
                        return Err(
                            "actual Cargo artifact has no unique declared target".to_owned()
                        );
                    }
                    let locators = SourceLocators {
                        declared: declared[0].src_path.clone(),
                        emitted: artifact.target.src_path.clone(),
                    };
                    let unit = Unit {
                        package: package.name.clone(),
                        version: package.version.clone(),
                        target: artifact.target.name.clone(),
                        kind: artifact.target.kind,
                        crate_types: artifact.target.crate_types,
                        features: artifact.features,
                        profile: artifact.profile,
                    };
                    if unit
                        .features
                        .windows(2)
                        .any(|features| features[0] >= features[1])
                        || !unit.profile.is_object()
                    {
                        return Err("Cargo unit has invalid features/profile".to_owned());
                    }
                    if source_locators
                        .insert((package.id.clone(), unit.key()?), locators.clone())
                        .is_some_and(|old| old != locators)
                    {
                        return Err("actual Cargo unit has conflicting source locators".to_owned());
                    }
                    let duplicate_executable = artifact
                        .executable
                        .filter(|_| unit.kind.iter().any(|kind| kind == "bin"))
                        .is_some_and(|path| {
                            executables
                                .insert((package.id.clone(), unit.target.clone()), path)
                                .is_some()
                        });
                    if duplicate_executable {
                        return Err("Cargo emitted duplicate executable artifacts".to_owned());
                    }
                    for filename in artifact.filenames {
                        if !filename.is_absolute() {
                            return Err("Cargo artifact filename is not absolute".to_owned());
                        }
                        let identity = (package.id.clone(), unit.clone());
                        if artifacts
                            .insert(filename, identity.clone())
                            .is_some_and(|old| old != identity)
                        {
                            return Err("Cargo artifact path has duplicate unit owners".to_owned());
                        }
                    }
                    units
                        .entry(package.id.clone())
                        .or_default()
                        .insert(unit.key()?, unit);
                }
                Some("build-script-executed") => {
                    if finished {
                        return Err(
                            "Cargo emitted a build-script result after build-finished".to_owned()
                        );
                    }
                    let owner = value
                        .get("package_id")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| "Cargo build-script result has no owner".to_owned())?;
                    if !self.packages.contains_key(owner) {
                        return Err("Cargo build-script owner is absent from metadata".to_owned());
                    }
                    let directory = value
                        .get("out_dir")
                        .and_then(serde_json::Value::as_str)
                        .map(PathBuf::from)
                        .filter(|path| path.is_absolute())
                        .ok_or_else(|| {
                            "Cargo build-script output is not an absolute locator".to_owned()
                        })?;
                    build_outputs
                        .entry(owner.to_owned())
                        .or_default()
                        .insert(directory);
                }
                Some(_) => {}
                None => return Err("Cargo output has no typed message reason".to_owned()),
            }
        }
        if !finished {
            return Err("Cargo artifact stream has no successful build-finished".to_owned());
        }
        self.units = units
            .into_iter()
            .map(|(id, units)| (id, units.into_values().collect()))
            .collect();
        self.executables = executables;
        self.artifacts = artifacts;
        self.source_locators = source_locators;
        self.build_outputs = build_outputs;
        Ok(())
    }

    pub fn named(&self, name: &str) -> Result<&Package, String> {
        let mut found = self
            .packages
            .values()
            .filter(|package| package.name == name && self.units.contains_key(&package.id));
        let package = found
            .next()
            .ok_or_else(|| format!("actual artifact build has no {name} package"))?;
        if found.next().is_some() {
            return Err(format!(
                "actual artifact build has multiple {name} packages"
            ));
        }
        Ok(package)
    }

    pub fn closure(&self, id: &str) -> Result<BTreeSet<String>, String> {
        let mut found = BTreeSet::new();
        let mut todo = vec![id.to_owned()];
        while let Some(id) = todo.pop() {
            if found.insert(id.clone()) {
                let deps = self
                    .edges
                    .get(&id)
                    .ok_or_else(|| format!("metadata has no resolve node {id}"))?;
                todo.extend(deps.iter().cloned());
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leased_manifest_alias_is_injective_and_unbuilt_targets_stay_declared() {
        let root = std::env::temp_dir().join(format!(
            "hermit-metadata-alias-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        struct Owned(PathBuf);
        impl Drop for Owned {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
        let _owned = Owned(root.clone());
        let leased = root.join("cargo-homes/validate-cargo-fixture");
        let package = leased.join("registry/src/pkg");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("Cargo.toml"),
            b"[package]\nname='fixture'\nversion='1.0.0'\n",
        )
        .unwrap();
        let alias = root.join("cargo-homes/slot-0");
        std::os::unix::fs::symlink(&leased, &alias).unwrap();
        let target = alias.join("registry/src/pkg/unbuilt-optional-test.rs");
        let value = Package {
            id: "fixture".to_owned(),
            name: "fixture".to_owned(),
            version: "1.0.0".to_owned(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_owned()),
            manifest_path: alias.join("registry/src/pkg/Cargo.toml"),
            targets: vec![Target {
                name: "optional".to_owned(),
                kind: vec!["test".to_owned()],
                crate_types: vec!["bin".to_owned()],
                src_path: target.clone(),
            }],
        };
        let measured = metadata_packages(vec![value.clone()]).unwrap();
        assert_eq!(
            measured["fixture"].manifest_path,
            package.join("Cargo.toml")
        );
        assert_eq!(measured["fixture"].targets[0].src_path, target);
        assert!(!target.exists(), "unbuilt target must not be manufactured");
        assert!(
            canonical_source_file(&target).is_err(),
            "an actual selected missing source still refuses"
        );
        let mut duplicate = value.clone();
        duplicate.id = "different-id".to_owned();
        duplicate.manifest_path = package.join("Cargo.toml");
        assert!(
            metadata_packages(vec![value.clone(), duplicate]).is_err(),
            "canonical manifest ownership must be injective"
        );
        std::fs::remove_file(package.join("Cargo.toml")).unwrap();
        assert!(
            metadata_packages(vec![value]).is_err(),
            "an actual manifest must exist"
        );
    }
}
