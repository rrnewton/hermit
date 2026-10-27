/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Actual rustc inputs for the compatibility producer. Cargo's artifact
//! profile is deliberately not treated as the complete compiler recipe.
//!
//! Raw Unix argv, response-file bytes and compiler identity remain in the
//! evidence bundle. Only the finite path/fingerprint rules below normalize
//! comparison inputs; every other argument is retained, in order. Unhandled
//! response-file expansion refuses a stamp rather than guessing its grammar.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use super::compatibility_graph::Graph;
use super::compatibility_graph::InputFile;
use super::compatibility_graph::Package;
use super::compatibility_graph::Unit;

pub const CAPTURE_ENV: &str = "HERMIT_COMPATIBILITY_RUSTC_CAPTURE";
pub const GATE_ENV: &str = "HERMIT_COMPATIBILITY_RUSTC_GATE";
pub const CAPTURE_SCHEMA: &str = "hermit-actual-rustc-capture/v1";
pub const NORMALIZATION: &str = "hermit-shared-rustc-semantics/v1";
pub const MAX_CAPTURE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_COMMAND_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_COMMANDS: usize = 4096;

/// Byte arrays avoid silently repairing non-UTF8 Unix paths/arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub bytes: u64,
    pub mode: u32,
    pub modified: (i64, i64),
    pub changed: (i64, i64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compiler {
    pub path: Vec<u8>,
    pub identity: FileIdentity,
    pub sha256: String,
    pub verbose_version: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub package: Option<String>,
    pub version: Option<String>,
    pub manifest_directory: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseFile {
    pub argument: usize,
    pub path: Vec<u8>,
    pub identity: FileIdentity,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invocation {
    pub schema: String,
    pub pid: u32,
    pub compiler: Compiler,
    pub cwd: Vec<u8>,
    pub argv: Vec<Vec<u8>>,
    pub context: Context,
    pub response_files: Vec<ResponseFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    pub schema: String,
    pub invocation_sha256: String,
    pub pid: u32,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub inputs_unchanged: bool,
    pub capture_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Captured {
    pub invocation: Invocation,
    pub completion: Completion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub schema: String,
    pub wrapper_sha256: String,
    pub target_directory: PathBuf,
    pub commands: Vec<Captured>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn full_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn path(bytes: &[u8]) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;
    if bytes.contains(&0) {
        return Err("captured Unix path contains NUL".to_owned());
    }
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

fn bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

fn absolute(cwd: &Path, raw: &[u8]) -> Result<PathBuf, String> {
    let value = path(raw)?;
    Ok(if value.is_absolute() {
        value
    } else {
        cwd.join(value)
    })
}

fn text(value: &[u8], owner: &str) -> Result<String, String> {
    String::from_utf8(value.to_vec())
        .map_err(|_| format!("non-UTF8 {owner} cannot be classified without guessing"))
}

impl Bundle {
    pub fn read(input: &InputFile) -> Result<Self, String> {
        let bundle: Self = serde_json::from_slice(&input.read(MAX_CAPTURE_BYTES)?)
            .map_err(|error| format!("invalid actual rustc capture: {error}"))?;
        if bundle.schema != CAPTURE_SCHEMA
            || bundle.commands.is_empty()
            || bundle.commands.len() > MAX_COMMANDS
            || !bundle.target_directory.is_absolute()
            || !full_digest(&bundle.wrapper_sha256)
        {
            return Err("unsupported or incomplete actual rustc capture".to_owned());
        }
        for captured in &bundle.commands {
            let command = &captured.invocation;
            let completion = &captured.completion;
            let encoded = serde_json::to_vec(command).map_err(|error| error.to_string())?;
            if command.schema != CAPTURE_SCHEMA
                || completion.schema != CAPTURE_SCHEMA
                || command.pid != completion.pid
                || encoded.len() > MAX_COMMAND_BYTES
                || digest(&encoded) != completion.invocation_sha256
                || completion.exit_code != Some(0)
                || completion.signal.is_some()
                || !completion.inputs_unchanged
                || completion.capture_error.is_some()
                || !full_digest(&command.compiler.sha256)
                || command.argv.iter().any(|argument| argument.contains(&0))
                || !path(&command.compiler.path)?.is_absolute()
                || !path(&command.cwd)?.is_absolute()
            {
                return Err(
                    "rustc capture contains an incomplete/failed/changed command".to_owned(),
                );
            }
            let version = std::str::from_utf8(&command.compiler.verbose_version)
                .map_err(|_| "captured compiler identity is non-UTF8".to_owned())?;
            if !version.lines().any(|line| {
                line.strip_prefix("commit-hash: ").is_some_and(|value| {
                    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
                })
            }) {
                return Err("captured compiler has no exact compiler commit".to_owned());
            }
            for response in &command.response_files {
                if digest(&response.bytes) != response.sha256
                    || command
                        .argv
                        .get(response.argument)
                        .is_none_or(|arg| !arg.starts_with(b"@"))
                {
                    return Err(
                        "captured response file is not bound to its actual argument".to_owned()
                    );
                }
            }
        }
        Ok(bundle)
    }
}

/// An artifact reference carries package/source, actual features and the
/// partial Cargo profile as additional dimensions. Complete codegen arguments
/// are bound separately for every compiled unit in the shared core closure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIdentity {
    package: String,
    version: String,
    source: String,
    unit: Unit,
}

fn package_source(package: &Package, core_root: &Path) -> Result<String, String> {
    // A source workspace package has source=null, while the identical core
    // package in an extension workspace has a Git source ID. Its separately
    // measured full core identity owns that distinction; compare the same
    // core-relative package in either actual Cargo graph.
    if let Ok(relative) = package.manifest_path.strip_prefix(core_root) {
        return Ok(format!(
            "core-source:{}",
            relative
                .to_str()
                .ok_or_else(|| "non-UTF8 core package path".to_owned())?
        ));
    }
    if let Some(source) = &package.source {
        return Ok(source.clone());
    }
    Err(format!(
        "shared package {} is a foreign path dependency without a measured source producer",
        package.name
    ))
}

fn artifact_identity(
    graph: &Graph,
    package: &str,
    unit: &Unit,
    core_root: &Path,
) -> Result<ArtifactIdentity, String> {
    let package = graph
        .packages
        .get(package)
        .ok_or_else(|| "artifact has no actual package".to_owned())?;
    Ok(ArtifactIdentity {
        package: package.name.clone(),
        version: package.version.clone(),
        source: package_source(package, core_root)?,
        unit: unit.clone(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum Argument {
    /// Includes unknown options/values unchanged; no omitted default is guessed.
    Raw {
        bytes: Vec<u8>,
    },
    Source {
        relative: Vec<u8>,
    },
    OutputDirectory,
    IncrementalDirectory,
    CargoFingerprint {
        option: String,
    },
    DependencyDirectory,
    Extern {
        name: Vec<u8>,
        artifact: Box<ArtifactIdentity>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Semantics {
    package: String,
    version: String,
    source: String,
    source_file: Vec<u8>,
    cargo_unit: Unit,
    compiler: Vec<u8>,
    compiler_sha256: String,
    arguments: Vec<Argument>,
    /// Every explicit -C argument is retained, in order. Defaults are bound
    /// by the exact compiler/target; absent options are not filled by guesses.
    explicit_codegen: Vec<Vec<u8>>,
}

fn option<'a>(
    argument: &'a [u8],
    name: &[u8],
    next: Option<&'a Vec<u8>>,
) -> Option<(&'a [u8], usize)> {
    if argument == name {
        return next.map(|value| (value.as_slice(), 2));
    }
    argument
        .strip_prefix(name)
        .and_then(|suffix| suffix.strip_prefix(b"="))
        .map(|value| (value, 1))
}

fn split_equal(value: &[u8]) -> Option<(&[u8], &[u8])> {
    value
        .iter()
        .position(|byte| *byte == b'=')
        .map(|index| (&value[..index], &value[index + 1..]))
}

fn actual_features(command: &Invocation) -> Result<Vec<String>, String> {
    let mut features = BTreeSet::new();
    for (index, argument) in command.argv.iter().enumerate() {
        let Some((cfg, _)) = option(argument, b"--cfg", command.argv.get(index + 1)) else {
            continue;
        };
        if let Some(feature) = cfg.strip_prefix(b"feature=\"") {
            let feature = feature
                .strip_suffix(b"\"")
                .ok_or_else(|| "actual feature cfg has unsupported quoting".to_owned())?;
            if !features.insert(text(feature, "actual feature cfg")?) {
                return Err("rustc repeats a feature cfg".to_owned());
            }
        }
    }
    Ok(features.into_iter().collect())
}

fn compiler_probe(command: &Invocation) -> bool {
    // Cargo can issue queries while package context is present. Do not treat
    // an actual emitted unit as a query merely because --print also appears.
    command.argv == [b"-vV".to_vec()]
        || (command
            .argv
            .iter()
            .any(|argument| argument == b"--print" || argument.starts_with(b"--print="))
            && !command.argv.iter().any(|argument| {
                matches!(argument.as_slice(), b"--emit" | b"--out-dir" | b"-o")
                    || argument.starts_with(b"--emit=")
                    || argument.starts_with(b"--out-dir=")
                    || (argument.starts_with(b"-o") && !argument.starts_with(b"--"))
            }))
}

/// Only the core CLI entry executable is absent from an extension's shared
/// library compilation. In particular, custom-build units also have a bin
/// crate type, and remain required along with every other core/dependency unit.
fn core_entry_binary(core: &Package, package: &Package, unit: &Unit) -> bool {
    if package.id != core.id
        || unit.target != "hermit"
        || unit.kind.len() != 1
        || unit.kind[0] != "bin"
        || unit.crate_types.len() != 1
        || unit.crate_types[0] != "bin"
    {
        return false;
    }
    let Some(crate_directory) = core.manifest_path.parent() else {
        return false;
    };
    let mut targets = package.targets.iter().filter(|target| {
        target.name == unit.target
            && target.kind == unit.kind
            && target.crate_types == unit.crate_types
    });
    targets
        .next()
        .is_some_and(|target| target.src_path == crate_directory.join("src/bin/hermit/main.rs"))
        && targets.next().is_none()
}

pub fn for_invocation(
    graph: &Graph,
    bundle: &Bundle,
    command: &Invocation,
) -> Result<Option<Semantics>, String> {
    if compiler_probe(command) {
        return Ok(None);
    }
    let manifest = command
        .context
        .manifest_directory
        .as_deref()
        .ok_or_else(|| "actual rustc unit has no Cargo manifest context".to_owned())?;
    let manifest = path(manifest)?
        .join("Cargo.toml")
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let matches: Vec<_> = graph
        .packages
        .values()
        .filter(|package| package.manifest_path == manifest)
        .collect();
    if matches.len() != 1 {
        return Err("rustc manifest context is not one actual Cargo package".to_owned());
    }
    let package = matches[0];
    if command.context.package.as_deref() != Some(package.name.as_str())
        || command.context.version.as_deref() != Some(package.version.as_str())
    {
        return Err("rustc Cargo name/version differs from its actual manifest package".to_owned());
    }
    let core = graph.named("hermit-run")?;
    let closure = graph.closure(&core.id)?;
    if !closure.contains(&package.id) {
        return Ok(None);
    }
    // The source/API producer supports only a full measured core checkout.
    let core_root = core
        .manifest_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "core manifest has no source root".to_owned())?;
    if !command.response_files.is_empty() || command.argv.iter().any(|arg| arg.starts_with(b"@")) {
        return Err("response-file bytes were captured, but verified rustc response expansion is not yet supported; compatibility is unavailable".to_owned());
    }
    let cwd = path(&command.cwd)?;
    let mut source = None;
    for (index, argument) in command.argv.iter().enumerate() {
        if argument.starts_with(b"-") {
            continue;
        }
        let candidate = absolute(&cwd, argument)?;
        let Some(target) = package
            .targets
            .iter()
            .find(|target| target.src_path == candidate)
        else {
            continue;
        };
        if source.replace((index, target)).is_some() {
            return Err("rustc source target is ambiguous in its original argv".to_owned());
        }
    }
    let (source_index, target) =
        source.ok_or_else(|| "rustc argv has no actual Cargo source target".to_owned())?;
    let relative = target
        .src_path
        .strip_prefix(package.manifest_path.parent().expect("manifest has parent"))
        .map_err(|_| "rustc source is outside its actual package".to_owned())?;
    let features = actual_features(command)?;
    let actual_units: Vec<_> = graph
        .units
        .get(&package.id)
        .into_iter()
        .flatten()
        .filter(|unit| {
            unit.target == target.name
                && unit.kind == target.kind
                && unit.crate_types == target.crate_types
                && unit.features == features
        })
        .collect();
    if actual_units.len() != 1 {
        return Err("actual rustc command is not one unique Cargo artifact unit".to_owned());
    }
    if core_entry_binary(core, package, actual_units[0]) {
        return Ok(None);
    }
    let mut output = None;
    for (index, argument) in command.argv.iter().enumerate() {
        let Some((value, _)) = option(argument, b"--out-dir", command.argv.get(index + 1)) else {
            continue;
        };
        if output.replace(absolute(&cwd, value)?).is_some() {
            return Err("rustc repeats its output directory".to_owned());
        }
    }
    let output = output.ok_or_else(|| "actual rustc unit has no output directory".to_owned())?;
    if !output.starts_with(&bundle.target_directory) {
        return Err("actual rustc output is outside the producer's fresh target".to_owned());
    }
    let artifact_directories: BTreeSet<_> = graph
        .artifacts
        .keys()
        .filter_map(|path| path.parent())
        .collect();
    let mut arguments = Vec::new();
    let mut explicit_codegen = Vec::new();
    let mut index = 0;
    while index < command.argv.len() {
        let argument = &command.argv[index];
        if index == source_index {
            arguments.push(Argument::Source {
                relative: bytes(relative),
            });
            index += 1;
            continue;
        }
        if let Some((_, count)) = option(argument, b"--out-dir", command.argv.get(index + 1)) {
            arguments.push(Argument::OutputDirectory);
            index += count;
            continue;
        }
        let codegen = if argument == b"-C" {
            command.argv.get(index + 1).map(|arg| (arg.as_slice(), 2))
        } else {
            argument
                .strip_prefix(b"-C")
                .filter(|value| !value.is_empty())
                .map(|value| (value, 1))
        };
        if let Some((value, count)) = codegen {
            if let Some(path) = value.strip_prefix(b"incremental=") {
                if !absolute(&cwd, path)?.starts_with(&bundle.target_directory) {
                    return Err("rustc incremental output is outside the fresh target".to_owned());
                }
                arguments.push(Argument::IncrementalDirectory);
            } else if let Some((name, setting)) = split_equal(value) {
                if matches!(name, b"metadata" | b"extra-filename") {
                    let hash = setting.strip_prefix(b"-").unwrap_or(setting);
                    if hash.is_empty() || !hash.iter().all(u8::is_ascii_hexdigit) {
                        return Err("non-Cargo rustc fingerprint cannot be normalized".to_owned());
                    }
                    arguments.push(Argument::CargoFingerprint {
                        option: text(name, "Cargo fingerprint")?,
                    });
                } else {
                    explicit_codegen.push(value.to_vec());
                    arguments.extend(
                        command.argv[index..index + count]
                            .iter()
                            .cloned()
                            .map(|bytes| Argument::Raw { bytes }),
                    );
                }
            } else {
                explicit_codegen.push(value.to_vec());
                arguments.extend(
                    command.argv[index..index + count]
                        .iter()
                        .cloned()
                        .map(|bytes| Argument::Raw { bytes }),
                );
            }
            index += count;
            continue;
        }
        if let Some((value, count)) = option(argument, b"--extern", command.argv.get(index + 1)) {
            let (name, filename) = split_equal(value).ok_or_else(|| {
                "unsupported rustc extern without a bound artifact path".to_owned()
            })?;
            let filename = absolute(&cwd, filename)?;
            let (owner, unit) = graph.artifacts.get(&filename).ok_or_else(|| {
                "rustc extern is not in the actual Cargo artifact inventory".to_owned()
            })?;
            arguments.push(Argument::Extern {
                name: name.to_vec(),
                artifact: Box::new(artifact_identity(graph, owner, unit, core_root)?),
            });
            index += count;
            continue;
        }
        let library = if argument == b"-L" {
            command.argv.get(index + 1).map(|arg| (arg.as_slice(), 2))
        } else {
            argument
                .strip_prefix(b"-L")
                .filter(|value| !value.is_empty())
                .map(|value| (value, 1))
        };
        if let Some((path, count)) = library
            .and_then(|(value, count)| value.strip_prefix(b"dependency=").map(|path| (path, count)))
        {
            let path = absolute(&cwd, path)?;
            if path.starts_with(&bundle.target_directory)
                && artifact_directories.contains(path.as_path())
            {
                arguments.push(Argument::DependencyDirectory);
                index += count;
                continue;
            }
        }
        arguments.push(Argument::Raw {
            bytes: argument.clone(),
        });
        index += 1;
    }
    Ok(Some(Semantics {
        package: package.name.clone(),
        version: package.version.clone(),
        source: package_source(package, core_root)?,
        source_file: bytes(relative),
        cargo_unit: actual_units[0].clone(),
        compiler: command.compiler.verbose_version.clone(),
        compiler_sha256: command.compiler.sha256.clone(),
        arguments,
        explicit_codegen,
    }))
}

/// Complete actual shared-unit population, not metadata's feature union. The
/// extension's own packages are allowed outside this measured core closure.
pub fn shared(graph: &Graph, bundle: &Bundle) -> Result<BTreeMap<String, Semantics>, String> {
    let mut result = BTreeMap::new();
    let mut units = BTreeSet::new();
    for captured in &bundle.commands {
        if let Some(semantics) = for_invocation(graph, bundle, &captured.invocation)? {
            let key = serde_json::to_string(&semantics).map_err(|error| error.to_string())?;
            units.insert((
                semantics.package.clone(),
                semantics.version.clone(),
                semantics.source.clone(),
                semantics.cargo_unit.key()?,
            ));
            if result.insert(key, semantics).is_some() {
                return Err(
                    "shared rustc unit was compiled more than once without a unique identity"
                        .to_owned(),
                );
            }
        }
    }
    let core = graph.named("hermit-run")?;
    let core_root = core
        .manifest_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "core manifest has no source root".to_owned())?;
    for id in graph.closure(&core.id)? {
        let package = &graph.packages[&id];
        for unit in graph.units.get(&id).into_iter().flatten() {
            if core_entry_binary(core, package, unit) {
                continue;
            }
            if !units.contains(&(
                package.name.clone(),
                package.version.clone(),
                package_source(package, core_root)?,
                unit.key()?,
            )) {
                return Err(format!(
                    "shared Cargo unit {}:{} has an artifact but no actual rustc command; cached population is not accepted",
                    package.name, unit.target
                ));
            }
        }
    }
    if result.is_empty() {
        return Err("no actual shared rustc units were captured".to_owned());
    }
    Ok(result)
}

pub fn cohort(
    first_graph: &Graph,
    first: &Bundle,
    second_graph: &Graph,
    second: &Bundle,
) -> Result<serde_json::Value, String> {
    if first.wrapper_sha256 != second.wrapper_sha256 {
        return Err("the two actual builds used different compiler capture executables".to_owned());
    }
    let compiler = first
        .commands
        .first()
        .ok_or_else(|| "the first compiler population is empty".to_owned())?;
    require_compiler(
        first,
        &compiler.invocation.compiler.verbose_version,
        &compiler.invocation.compiler.sha256,
    )?;
    require_compiler(
        second,
        &compiler.invocation.compiler.verbose_version,
        &compiler.invocation.compiler.sha256,
    )?;
    let expected = shared(first_graph, first)?;
    let observed = shared(second_graph, second)?;
    if expected != observed {
        return Err(
            "the two actual builds have different shared rustc semantics/populations".to_owned(),
        );
    }
    let bytes = serde_json::to_vec(&expected).map_err(|error| error.to_string())?;
    Ok(
        serde_json::json!({"schema":CAPTURE_SCHEMA,"normalization":NORMALIZATION,
        "shared_units":expected.len(),"shared_sha256":digest(&bytes)}),
    )
}

/// The stamp's presentation compiler identity must be the same actual
/// frontend measured before/after every command, not a separate tool on PATH.
pub fn require_compiler(bundle: &Bundle, version: &[u8], sha256: &str) -> Result<(), String> {
    if !full_digest(sha256)
        || bundle.commands.iter().any(|captured| {
            captured.invocation.compiler.verbose_version != version
                || captured.invocation.compiler.sha256 != sha256
        })
    {
        return Err(
            "stamp compiler identity differs from the actual captured compiler frontend".to_owned(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic Cargo/capture fixtures exercise the actual normalizer; they
    /// do not execute a compiler, producer, probe, Hermit or guest.
    #[test]
    fn shared_library_cohort_excludes_only_core_entry_and_requires_every_other_unit() {
        struct FixtureDirectory(PathBuf);
        impl Drop for FixtureDirectory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = std::env::temp_dir().join(format!(
            "hermit-rustc-cohort-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let _fixture = FixtureDirectory(root.clone());
        let target_directory = root.join("target");
        let definitions = [
            (
                "core",
                "hermit-run",
                "hermit",
                "lib",
                "lib",
                "hermit-cli/src/lib.rs",
            ),
            (
                "core",
                "hermit-run",
                "build-script-build",
                "custom-build",
                "bin",
                "hermit-cli/build.rs",
            ),
            (
                "dependency",
                "shared-dependency",
                "shared_dependency",
                "lib",
                "lib",
                "shared-dependency/src/lib.rs",
            ),
            (
                "build-dependency",
                "build-dependency",
                "build_dependency",
                "lib",
                "lib",
                "build-dependency/src/lib.rs",
            ),
            (
                "core",
                "hermit-run",
                "hermit-dap",
                "bin",
                "bin",
                "hermit-cli/src/bin/hermit-dap/main.rs",
            ),
            (
                "core",
                "hermit-run",
                "hermit",
                "bin",
                "bin",
                "hermit-cli/src/bin/hermit/main.rs",
            ),
        ];
        let packages = [
            ("core", "hermit-run", "hermit-cli"),
            ("dependency", "shared-dependency", "shared-dependency"),
            ("build-dependency", "build-dependency", "build-dependency"),
        ];
        for (_, _, _, _, _, source) in &definitions {
            let path = root.join(source);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "// synthetic normalizer fixture\n").unwrap();
        }
        for (_, name, directory) in &packages {
            std::fs::write(
                root.join(directory).join("Cargo.toml"),
                format!("[package]\nname = {name:?}\nversion = \"0.5.4\"\n"),
            )
            .unwrap();
        }
        let manifest = root.join("Cargo.toml");
        let lock = root.join("Cargo.lock");
        std::fs::write(&manifest, "[workspace]\n").unwrap();
        std::fs::write(&lock, "version = 4\n").unwrap();
        let metadata = serde_json::json!({
            "version": 1,
            "workspace_root": root,
            "packages": packages.iter().map(|(id, name, directory)| {
                serde_json::json!({
                    "id": id, "name": name, "version": "0.5.4", "source": null,
                    "manifest_path": root.join(directory).join("Cargo.toml"),
                    "targets": definitions.iter().filter(|definition| definition.0 == *id)
                        .map(|(_, _, name, kind, crate_type, source)| serde_json::json!({
                            "name": name, "kind": [kind], "crate_types": [crate_type],
                            "src_path": root.join(source),
                        })).collect::<Vec<_>>(),
                })
            }).collect::<Vec<_>>(),
            "resolve": {"nodes": [
                {"id":"core", "deps":[
                    {"pkg":"dependency", "dep_kinds":[{"kind":null}]},
                    {"pkg":"build-dependency", "dep_kinds":[{"kind":"build"}]},
                ]},
                {"id":"dependency", "deps":[]},
                {"id":"build-dependency", "deps":[]},
            ]},
        });
        let metadata_path = root.join("metadata.json");
        std::fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let input_file = |path: &Path| InputFile {
            path: path.to_owned(),
            sha256: digest(&std::fs::read(path).unwrap()),
        };
        let profile = serde_json::json!({
            "opt_level":"0", "debuginfo":2, "debug_assertions":true,
            "overflow_checks":true, "test":false,
        });
        let graph_for = |include_core_entry: bool| {
            let length = definitions.len() - usize::from(!include_core_entry);
            let mut stream = Vec::new();
            for (index, (id, _, name, kind, crate_type, source)) in
                definitions[..length].iter().enumerate()
            {
                let filename = target_directory.join(format!("unit-{index}"));
                let artifact = serde_json::json!({
                    "reason":"compiler-artifact", "package_id":id,
                    "target":{"name":name,"kind":[kind],"crate_types":[crate_type],"src_path":root.join(source)},
                    "features":[], "profile":profile,
                    "filenames":[filename], "executable":null,
                });
                stream.extend(serde_json::to_vec(&artifact).unwrap());
                stream.push(b'\n');
            }
            stream.extend(b"{\"reason\":\"build-finished\",\"success\":true}\n");
            let path = root.join(if include_core_entry {
                "artifacts-with-cli.jsonl"
            } else {
                "artifacts-library.jsonl"
            });
            std::fs::write(&path, stream).unwrap();
            let artifacts = input_file(&path);
            let input = super::super::compatibility_graph::BuildInput {
                schema: super::super::compatibility_graph::INPUT_SCHEMA.to_owned(),
                metadata: input_file(&metadata_path),
                first_artifacts: artifacts.clone(),
                artifacts: artifacts.clone(),
                rustc_first: artifacts.clone(),
                rustc_second: artifacts.clone(),
                source_snapshot: artifacts,
                manifest: input_file(&manifest),
                lock: input_file(&lock),
                package: "hermit-run".to_owned(),
                binary: "hermit".to_owned(),
                target: "x86_64-unknown-linux-gnu".to_owned(),
                profile: "dev".to_owned(),
            };
            Graph::read(&input).unwrap()
        };
        let compiler = Compiler {
            path: bytes(&root.join("fixture-rustc")),
            identity: FileIdentity {
                device: 0,
                inode: 0,
                bytes: 1,
                mode: 0,
                modified: (0, 0),
                changed: (0, 0),
            },
            sha256: "a".repeat(64),
            verbose_version: format!("rustc fixture\ncommit-hash: {}\n", "b".repeat(40))
                .into_bytes(),
        };
        let captures = definitions
            .iter()
            .enumerate()
            .map(|(index, (id, name, target, _, crate_type, source))| {
                let directory = packages.iter().find(|package| package.0 == *id).unwrap().2;
                let invocation = Invocation {
                    schema: CAPTURE_SCHEMA.to_owned(),
                    pid: index as u32 + 1,
                    compiler: compiler.clone(),
                    cwd: bytes(&root),
                    argv: vec![
                        b"--crate-name".to_vec(),
                        target.replace('-', "_").into_bytes(),
                        bytes(&root.join(source)),
                        b"--crate-type".to_vec(),
                        crate_type.as_bytes().to_vec(),
                        b"--emit=link".to_vec(),
                        b"--out-dir".to_vec(),
                        bytes(&target_directory.join("debug/deps")),
                        b"-C".to_vec(),
                        b"opt-level=0".to_vec(),
                    ],
                    context: Context {
                        package: Some((*name).to_owned()),
                        version: Some("0.5.4".to_owned()),
                        manifest_directory: Some(bytes(&root.join(directory))),
                    },
                    response_files: vec![],
                };
                let completion = Completion {
                    schema: CAPTURE_SCHEMA.to_owned(),
                    invocation_sha256: digest(&serde_json::to_vec(&invocation).unwrap()),
                    pid: invocation.pid,
                    exit_code: Some(0),
                    signal: None,
                    inputs_unchanged: true,
                    capture_error: None,
                };
                Captured {
                    invocation,
                    completion,
                }
            })
            .collect::<Vec<_>>();
        let with_entry = Bundle {
            schema: CAPTURE_SCHEMA.to_owned(),
            wrapper_sha256: "c".repeat(64),
            target_directory: target_directory.clone(),
            commands: captures,
        };
        let mut library_only = with_entry.clone();
        library_only.commands.pop();
        let core_graph = graph_for(true);
        let extension_graph = graph_for(false);
        let expected = shared(&core_graph, &with_entry).unwrap();
        assert_eq!(expected.len(), definitions.len() - 1);
        assert_eq!(expected, shared(&extension_graph, &library_only).unwrap());
        assert!(cohort(&core_graph, &with_entry, &extension_graph, &library_only).is_ok());
        assert!(
            for_invocation(
                &core_graph,
                &with_entry,
                &with_entry.commands.last().unwrap().invocation
            )
            .unwrap()
            .is_none()
        );
        // A core CLI artifact does not require an executable-only capture in
        // the shared library cohort, but every other unit is still mandatory.
        assert_eq!(expected, shared(&core_graph, &library_only).unwrap());
        for index in 0..library_only.commands.len() {
            let mut missing = library_only.clone();
            missing.commands.remove(index);
            assert!(
                shared(&extension_graph, &missing)
                    .unwrap_err()
                    .contains("cached population is not accepted")
            );
            let mut changed = library_only.clone();
            changed.commands[index]
                .invocation
                .argv
                .extend([b"-C".to_vec(), b"opt-level=3".to_vec()]);
            changed.commands[index].completion.invocation_sha256 =
                digest(&serde_json::to_vec(&changed.commands[index].invocation).unwrap());
            assert!(cohort(&core_graph, &with_entry, &extension_graph, &changed).is_err());
        }
    }
}
