use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use sha2::Digest as _;
use sha2::Sha256;

const BUILD_ENVIRONMENT: &[&str] = &[
    "CARGO_ENCODED_RUSTFLAGS",
    "DEBUG",
    "HOST",
    "OPT_LEVEL",
    "PROFILE",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTFLAGS",
    "TARGET",
];

fn source_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()))
        .map(|entry| entry.expect("failed to read Detcore source entry").path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            source_files(root, &path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
}

fn hash_value(hash: &mut Sha256, name: &[u8], value: &[u8]) {
    hash.update((name.len() as u64).to_le_bytes());
    hash.update(name);
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value);
}

fn hash_file(hash: &mut Sha256, name: &[u8], path: &Path) {
    let contents =
        fs::read(path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    hash_value(hash, name, &contents);
}

fn package_label(package: &Value) -> String {
    let name = package["name"]
        .as_str()
        .expect("package name is not a string");
    let version = package["version"]
        .as_str()
        .expect("package version is not a string");
    let source = package["source"].as_str().unwrap_or("path");
    format!("{name}@{version} ({source})")
}

fn non_dev_dependency(dependency: &Value) -> bool {
    dependency["dep_kinds"].as_array().is_none_or(|kinds| {
        kinds.is_empty()
            || kinds
                .iter()
                .any(|kind| kind["kind"].as_str() != Some("dev"))
    })
}

fn canonical_resolved_graph(metadata: &Value, manifest: &Path) -> (Vec<u8>, Vec<PathBuf>) {
    let packages = metadata["packages"]
        .as_array()
        .expect("cargo metadata omitted packages");
    let packages_by_id = packages
        .iter()
        .map(|package| {
            (
                package["id"]
                    .as_str()
                    .expect("package id is not a string")
                    .to_owned(),
                package,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let root_id = packages
        .iter()
        .find(|package| {
            package["manifest_path"]
                .as_str()
                .is_some_and(|path| Path::new(path) == manifest)
        })
        .and_then(|package| package["id"].as_str())
        .expect("cargo metadata omitted the Detcore package");
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .expect("cargo metadata omitted resolved nodes");
    let nodes_by_id = nodes
        .iter()
        .map(|node| {
            (
                node["id"]
                    .as_str()
                    .expect("resolved node id is not a string")
                    .to_owned(),
                node,
            )
        })
        .collect::<BTreeMap<_, _>>();

    let mut reachable = BTreeSet::from([root_id.to_owned()]);
    let mut pending = vec![root_id.to_owned()];
    let mut edges = Vec::new();
    while let Some(id) = pending.pop() {
        let node = nodes_by_id
            .get(&id)
            .unwrap_or_else(|| panic!("resolved graph omitted node {id}"));
        for dependency in node["deps"]
            .as_array()
            .expect("resolved node omitted dependencies")
            .iter()
            .filter(|dependency| non_dev_dependency(dependency))
        {
            let target = dependency["pkg"]
                .as_str()
                .expect("resolved dependency package is not a string")
                .to_owned();
            edges.push((
                id.clone(),
                dependency["name"].as_str().unwrap_or_default().to_owned(),
                target.clone(),
                serde_json::to_string(&dependency["dep_kinds"])
                    .expect("failed to canonicalize dependency kinds"),
            ));
            if reachable.insert(target.clone()) {
                pending.push(target);
            }
        }
    }

    let mut graph = Vec::new();
    let mut local_roots = Vec::new();
    let mut labels = BTreeMap::new();
    for id in &reachable {
        let package = packages_by_id
            .get(id)
            .unwrap_or_else(|| panic!("cargo metadata omitted package {id}"));
        let label = package_label(package);
        labels.insert(id.clone(), label.clone());
        let mut features = nodes_by_id[id]["features"]
            .as_array()
            .expect("resolved node omitted features")
            .iter()
            .map(|feature| feature.as_str().expect("feature is not a string"))
            .collect::<Vec<_>>();
        features.sort_unstable();
        graph.extend_from_slice(format!("package\0{label}\0{}\n", features.join(",")).as_bytes());
        if package["source"].is_null() {
            let manifest = PathBuf::from(
                package["manifest_path"]
                    .as_str()
                    .expect("path package omitted manifest"),
            );
            local_roots.push(
                manifest
                    .parent()
                    .expect("manifest has no parent")
                    .to_owned(),
            );
        }
    }
    edges.sort_by(|left, right| {
        (&labels[&left.0], &left.1, &labels[&left.2], &left.3).cmp(&(
            &labels[&right.0],
            &right.1,
            &labels[&right.2],
            &right.3,
        ))
    });
    for (from, name, to, kinds) in edges {
        graph.extend_from_slice(
            format!(
                "dependency\0{}\0{name}\0{}\0{kinds}\n",
                labels[&from], labels[&to]
            )
            .as_bytes(),
        );
    }
    local_roots.sort();
    local_roots.dedup();
    (graph, local_roots)
}

fn resolved_graph(root: &Path) -> (Vec<u8>, Vec<PathBuf>) {
    let manifest = root.join("Cargo.toml");
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let target = env::var_os("TARGET").expect("Cargo did not set TARGET");
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked"])
        .arg("--filter-platform")
        .arg(target)
        .arg("--manifest-path")
        .arg(&manifest)
        .output()
        .expect("failed to run cargo metadata for the Detcore dependency graph");
    assert!(
        output.status.success(),
        "cargo metadata failed while resolving the Detcore dependency graph: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata returned invalid JSON");
    canonical_resolved_graph(&metadata, &manifest)
}

fn hash_local_package(hash: &mut Sha256, root: &Path) {
    let package = fs::read_to_string(root.join("Cargo.toml"))
        .unwrap_or_else(|error| panic!("failed to read {}/Cargo.toml: {error}", root.display()));
    let name = package
        .lines()
        .find_map(|line| line.trim().strip_prefix("name = \"")?.strip_suffix('"'))
        .expect("local package manifest has no package name");
    hash_file(
        hash,
        format!("local-package/{name}/Cargo.toml").as_bytes(),
        &root.join("Cargo.toml"),
    );
    let build = root.join("build.rs");
    if build.is_file() {
        hash_file(
            hash,
            format!("local-package/{name}/build.rs").as_bytes(),
            &build,
        );
    }
    let source = root.join("src");
    if source.is_dir() {
        let mut files = Vec::new();
        source_files(root, &source, &mut files);
        files.sort();
        for relative in files {
            hash_file(
                hash,
                format!("local-package/{name}/{}", relative.display()).as_bytes(),
                &root.join(relative),
            );
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src");

    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let mut files = vec![PathBuf::from("Cargo.toml"), PathBuf::from("build.rs")];
    source_files(&root, &root.join("src"), &mut files);
    files.sort();

    let mut hash = Sha256::new();
    for relative in files {
        hash_file(
            &mut hash,
            relative.as_os_str().as_encoded_bytes(),
            &root.join(&relative),
        );
    }

    let (graph, local_packages) = resolved_graph(&root);
    hash_value(&mut hash, b"resolved-dependency-graph", &graph);
    for package in local_packages {
        if package != root {
            println!("cargo:rerun-if-changed={}", package.display());
            hash_local_package(&mut hash, &package);
        }
    }

    for name in BUILD_ENVIRONMENT {
        println!("cargo:rerun-if-env-changed={name}");
        hash_value(
            &mut hash,
            name.as_bytes(),
            env::var_os(name)
                .as_deref()
                .unwrap_or_else(|| OsStr::new("<unset>"))
                .as_encoded_bytes(),
        );
    }
    let mut cargo_cfg = env::vars_os()
        .filter(|(name, _)| name.as_encoded_bytes().starts_with(b"CARGO_CFG_"))
        .collect::<Vec<_>>();
    cargo_cfg.sort();
    for (name, value) in cargo_cfg {
        hash_value(&mut hash, name.as_encoded_bytes(), value.as_encoded_bytes());
    }
    let mut features = env::vars_os()
        .filter(|(name, _)| name.as_encoded_bytes().starts_with(b"CARGO_FEATURE_"))
        .collect::<Vec<_>>();
    features.sort();
    for (name, value) in features {
        hash_value(&mut hash, name.as_encoded_bytes(), value.as_encoded_bytes());
    }
    let rustc = Command::new(env::var_os("RUSTC").unwrap())
        .arg("-vV")
        .output()
        .expect("failed to query rustc identity");
    assert!(rustc.status.success(), "rustc -vV failed");
    hash_value(&mut hash, b"rustc -vV", &rustc.stdout);

    let identity = hex::encode(hash.finalize());
    let generated = format!(
        "/// Exact identity of Detcore source, resolved non-dev dependencies, features, target, profile, compiler, and compiler flags.\n\
         pub const DETCORE_BUILD_ID: &str = \"{identity}\";\n"
    );
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("detcore-build-id.rs"),
        generated,
    )
    .expect("failed to write Detcore build identity");
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn graph(version: &str) -> Value {
        let dep_id = format!("registry+https://example.invalid#index@{version}");
        json!({
            "packages": [
                {
                    "name": "detcore", "version": "0.2.0", "id": "detcore-id",
                    "source": null, "manifest_path": "/workspace/detcore/Cargo.toml"
                },
                {
                    "name": "index", "version": version, "id": dep_id,
                    "source": "registry+https://example.invalid", "manifest_path": "/registry/index/Cargo.toml"
                }
            ],
            "resolve": { "nodes": [
                {
                    "id": "detcore-id", "features": [],
                    "deps": [{ "name": "index", "pkg": dep_id, "dep_kinds": [{"kind": null, "target": null}] }]
                },
                { "id": dep_id, "features": ["std"], "deps": [] }
            ]}
        })
    }

    #[test]
    fn resolved_dependency_change_changes_identity_input() {
        let manifest = Path::new("/workspace/detcore/Cargo.toml");
        let first = canonical_resolved_graph(&graph("1.0.0"), manifest).0;
        let second = canonical_resolved_graph(&graph("1.0.1"), manifest).0;
        assert_ne!(first, second);
    }

    #[test]
    fn dev_only_dependency_is_not_part_of_shipping_identity() {
        let mut metadata = graph("1.0.0");
        metadata["resolve"]["nodes"][0]["deps"][0]["dep_kinds"][0]["kind"] = json!("dev");
        let manifest = Path::new("/workspace/detcore/Cargo.toml");
        let canonical = canonical_resolved_graph(&metadata, manifest).0;
        assert!(
            !String::from_utf8(canonical)
                .unwrap()
                .contains("index@1.0.0")
        );
    }
}
