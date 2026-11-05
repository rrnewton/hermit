//! `#51 container-prompt-files`: the image build receives every file compiled into the server.
//!
//! Rust resolves `include_str!` and `include_bytes!` while compiling. A normal checkout can
//! therefore build while a container context fails: Docker sees only the paths copied into the
//! build stage. Keep an inventory here so a new compile-time asset cannot silently create that
//! split.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn rust_sources_below(path: &Path, out: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| panic!("could not enumerate {}: {error}", path.display()));
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            rust_sources_below(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

fn literal_include_paths(source: &str) -> impl Iterator<Item = &str> {
    ["include_str!(\"", "include_bytes!(\""]
        .into_iter()
        .flat_map(move |opening| {
            source.match_indices(opening).map(move |(start, _)| {
                let value = &source[start + opening.len()..];
                value
                    .split_once('"')
                    .unwrap_or_else(|| panic!("unterminated {opening} in Rust source"))
                    .0
            })
        })
}

fn production_compile_time_inputs(root: &Path) -> BTreeSet<PathBuf> {
    let canonical_root = root
        .canonicalize()
        .unwrap_or_else(|error| panic!("could not resolve {}: {error}", root.display()));
    let mut sources = Vec::new();
    rust_sources_below(&root.join("src"), &mut sources);

    let mut inputs = BTreeSet::new();
    for source_path in sources {
        let source = fs::read_to_string(&source_path)
            .unwrap_or_else(|error| panic!("could not read {}: {error}", source_path.display()));
        // Source tests live in a trailing cfg(test) module. They compile from the host checkout,
        // not in `cargo build --release --bin vibe-talk`, so their fixtures are not image inputs.
        let production = source.split("#[cfg(test)]").next().unwrap_or(&source);
        for included in literal_include_paths(production) {
            let absolute = source_path
                .parent()
                .expect("a Rust source has a parent")
                .join(included)
                .canonicalize()
                .unwrap_or_else(|error| {
                    panic!(
                        "{} includes {included:?}, which cannot be resolved: {error}",
                        source_path.display()
                    )
                });
            inputs.insert(
                absolute
                    .strip_prefix(&canonical_root)
                    .unwrap_or_else(|_| {
                        panic!(
                            "{} includes a file outside the image build context: {}",
                            source_path.display(),
                            absolute.display()
                        )
                    })
                    .to_path_buf(),
            );
        }
    }
    inputs
}

fn build_stage_copy_sources(containerfile: &str) -> Vec<PathBuf> {
    let mut in_build_stage = false;
    let mut sources = Vec::new();

    for line in containerfile.lines().map(str::trim) {
        if line.starts_with("FROM ") {
            if in_build_stage {
                break;
            }
            in_build_stage = true;
            continue;
        }
        if !in_build_stage || !line.starts_with("COPY ") || line.contains("--from=") {
            continue;
        }

        let fields = line.split_whitespace().skip(1).collect::<Vec<_>>();
        assert!(
            fields.len() >= 2,
            "build-stage COPY needs at least one source and one destination: {line}"
        );
        sources.extend(
            fields[..fields.len() - 1]
                .iter()
                .filter(|field| !field.starts_with("--"))
                .map(|field| PathBuf::from(field.trim_start_matches("./").trim_end_matches('/'))),
        );
    }
    sources
}

#[test]
fn build_stage_copies_every_compile_time_input() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inputs = production_compile_time_inputs(root);
    assert!(
        !inputs.is_empty(),
        "the compile-time input inventory is empty"
    );

    let containerfile = fs::read_to_string(root.join("Containerfile"))
        .expect("the image recipe should be readable");
    let copied = build_stage_copy_sources(&containerfile);
    let missing = inputs
        .iter()
        .filter(|input| !copied.iter().any(|source| input.starts_with(source)))
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();

    assert!(
        missing.is_empty(),
        "Containerfile's build stage does not copy compile-time input(s): {}",
        missing.join(", ")
    );
}
