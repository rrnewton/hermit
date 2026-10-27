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
        let mut packages = BTreeMap::new();
        for package in metadata.packages {
            if packages.insert(package.id.clone(), package).is_some() {
                return Err("actual metadata repeats a package ID".to_owned());
            }
        }
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
        };
        graph.read_artifacts(&input.artifacts.read(MAX_GRAPH_BYTES)?)?;
        Ok(graph)
    }

    pub fn read_artifacts(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut units: BTreeMap<String, BTreeMap<String, Unit>> = BTreeMap::new();
        let mut executables = BTreeMap::new();
        let mut artifacts = BTreeMap::new();
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
