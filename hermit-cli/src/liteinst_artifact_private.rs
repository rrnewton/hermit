use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

use super::digest;
use super::invalid;
use super::require;

pub const RUNTIME_NAME: &str = "hermit_liteinst_detcore_private.elf";
pub const INPUT_FILES: &[&str] = &[
    "cc",
    "cc1",
    "as",
    "ld.bfd",
    "private-rcrt1.o",
    "crti.o",
    "crtbeginS.o",
    "crtendS.o",
    "crtn.o",
    "libutil.a",
    "librt.a",
    "libpthread.a",
    "libm.a",
    "libmvec.a",
    "libdl.a",
    "libc.a",
    "libgcc_eh.a",
    "libgcc.a",
];

pub fn requested() -> io::Result<bool> {
    match std::env::var("HERMIT_LITEINST_RUNTIME_KIND").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("preload") => Ok(false),
        Ok("private-crt") => Ok(true),
        _ => Err(invalid("unsupported LiteInst runtime build kind")),
    }
}

pub struct NativeInputs {
    pub files: std::collections::BTreeMap<String, PathBuf>,
    pub headers: std::collections::BTreeMap<String, PathBuf>,
    pub identity: Value,
}

pub fn header_tree(root: &Path) -> io::Result<Value> {
    let mut directories = vec![(root.to_path_buf(), 0)];
    let mut entries = serde_json::Map::new();
    let mut total = 0u64;
    while let Some((directory, depth)) = directories.pop() {
        require(depth < 32, "native header tree is too deep")?;
        require(
            fs::symlink_metadata(&directory)?.is_dir(),
            "native header root must be a directory",
        )?;
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                directories.push((path, depth + 1));
            } else {
                require(
                    metadata.is_file() && metadata.len() <= 8 * 1024 * 1024,
                    "native header must be bounded regular file",
                )?;
                total = total
                    .checked_add(metadata.len())
                    .ok_or_else(|| invalid("native header size overflow"))?;
                require(
                    total <= 128 * 1024 * 1024 && entries.len() < 8192,
                    "native header tree is too large",
                )?;
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| invalid("header escaped root"))?
                    .to_str()
                    .ok_or_else(|| invalid("non-UTF8 header path"))?;
                let bytes = fs::read(&path)?;
                entries.insert(
                    relative.to_owned(),
                    serde_json::json!({"sha256": digest(&bytes), "size": bytes.len()}),
                );
            }
        }
    }
    require(!entries.is_empty(), "empty native header tree")?;
    Ok(Value::Object(entries))
}

pub fn native_inputs(path: &Path) -> io::Result<NativeInputs> {
    require(
        fs::symlink_metadata(path)?.is_file(),
        "native input manifest must be regular",
    )?;
    let record: Value = serde_json::from_slice(&fs::read(path)?)?;
    let object = record
        .as_object()
        .ok_or_else(|| invalid("native input manifest must be object"))?;
    require(
        object.len() == 3 && record["schema"] == 1,
        "native input manifest schema mismatch",
    )?;
    let entries = record["files"]
        .as_object()
        .ok_or_else(|| invalid("native input files missing"))?;
    require(
        entries.len() == INPUT_FILES.len(),
        "native input file roles mismatch",
    )?;
    let mut files = std::collections::BTreeMap::new();
    let mut identities = serde_json::Map::new();
    for name in INPUT_FILES {
        let entry = entries
            .get(*name)
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("missing native input role"))?;
        require(entry.len() == 2, "native input fields mismatch")?;
        let path = PathBuf::from(
            entry
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("missing native path"))?,
        );
        require(path.is_absolute(), "native input path must be absolute")?;
        let path = path.canonicalize()?;
        require(
            fs::symlink_metadata(&path)?.is_file(),
            "native input must be regular",
        )?;
        let bytes = fs::read(&path)?;
        let hash = digest(&bytes);
        require(
            !bytes.is_empty() && entry.get("sha256").and_then(Value::as_str) == Some(&hash),
            "native input bytes disagree with declared identity",
        )?;
        identities.insert(
            (*name).to_owned(),
            serde_json::json!({"sha256": hash, "size": bytes.len()}),
        );
        files.insert((*name).to_owned(), path);
    }
    let header_roles = record["headers"]
        .as_object()
        .ok_or_else(|| invalid("native header roles missing"))?;
    require(header_roles.len() == 2, "native header roles mismatch")?;
    let mut headers = std::collections::BTreeMap::new();
    let mut header_identities = serde_json::Map::new();
    for role in ["gcc", "system"] {
        let entry = header_roles
            .get(role)
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("native header role missing"))?;
        require(entry.len() == 2, "native header fields mismatch")?;
        let path = PathBuf::from(
            entry
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("native header path missing"))?,
        );
        require(path.is_absolute(), "native header path must be absolute")?;
        let path = path.canonicalize()?;
        let tree = header_tree(&path)?;
        require(
            entry.get("sha256").and_then(Value::as_str)
                == Some(&digest(&serde_json::to_vec(&tree)?)),
            "native header tree differs from declared identity",
        )?;
        headers.insert(role.to_owned(), path);
        header_identities.insert(role.to_owned(), tree);
    }
    Ok(NativeInputs {
        files,
        headers,
        identity: serde_json::json!({"schema": 1, "files": identities, "headers": header_identities}),
    })
}

pub fn environment_inputs() -> io::Result<NativeInputs> {
    let path = std::env::var_os("HERMIT_LITEINST_PRIVATE_INPUTS")
        .ok_or_else(|| invalid("private GNU/native input manifest is required"))?;
    native_inputs(Path::new(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_manifest_binds_every_role_and_rejects_changed_bytes() {
        let root =
            std::env::temp_dir().join(format!("private-native-inputs-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let file = root.join("ordinary-host-data");
        fs::write(&file, b"not an executable").unwrap();
        let mut files = serde_json::Map::new();
        for name in INPUT_FILES {
            files.insert(
                (*name).to_owned(),
                serde_json::json!({"path":file, "sha256":digest(b"not an executable")}),
            );
        }
        let manifest = root.join("manifest.json");
        let headers = root.join("headers");
        fs::create_dir(&headers).unwrap();
        fs::write(headers.join("stddef.h"), b"ordinary header data").unwrap();
        let header = serde_json::json!({"path": headers, "sha256": digest(&serde_json::to_vec(&header_tree(&headers).unwrap()).unwrap())});
        let mut record =
            serde_json::json!({"schema":1,"files":files,"headers":{"gcc":header,"system":header}});
        fs::write(&manifest, serde_json::to_vec(&record).unwrap()).unwrap();
        let first = native_inputs(&manifest).unwrap();
        assert_eq!(first.files.len(), INPUT_FILES.len());
        fs::write(headers.join("stddef.h"), b"changed header").unwrap();
        assert!(native_inputs(&manifest).is_err());
        fs::write(headers.join("stddef.h"), b"ordinary header data").unwrap();
        fs::write(&file, b"changed").unwrap();
        assert!(native_inputs(&manifest).is_err());
        fs::write(&file, b"not an executable").unwrap();
        record["files"].as_object_mut().unwrap().remove("cc");
        fs::write(&manifest, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(native_inputs(&manifest).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
