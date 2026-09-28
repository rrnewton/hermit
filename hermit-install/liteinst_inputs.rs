use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn provenance_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".provenance.json");
    PathBuf::from(name)
}

fn read_regular(path: &Path) -> io::Result<(Vec<u8>, fs::Permissions)> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(invalid(format!(
            "LiteInst input is not a regular file: {}",
            path.display()
        )));
    }
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(invalid(format!(
            "LiteInst input is not a regular file: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Err(invalid(format!(
            "LiteInst input is empty: {}",
            path.display()
        )));
    }
    Ok((bytes, metadata.permissions()))
}

pub struct RuntimePair {
    runtime: Vec<u8>,
    provenance: Vec<u8>,
    permissions: fs::Permissions,
}

impl RuntimePair {
    pub fn read(
        runtime: &Path,
        validate: impl FnOnce(&[u8], &[u8]) -> io::Result<()>,
    ) -> io::Result<Self> {
        let (runtime_bytes, permissions) = read_regular(runtime)?;
        let (provenance, _) = read_regular(&provenance_path(runtime))?;
        validate(&runtime_bytes, &provenance)?;
        Ok(Self {
            runtime: runtime_bytes,
            provenance,
            permissions,
        })
    }

    pub fn install(
        &self,
        destination: &Path,
        validate: impl FnOnce(&[u8], &[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut runtime = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination)?;
        runtime.write_all(&self.runtime)?;
        runtime.set_permissions(self.permissions.clone())?;
        runtime.sync_all()?;

        let provenance_path = provenance_path(destination);
        let mut provenance = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&provenance_path)?;
        provenance.write_all(&self.provenance)?;
        provenance.sync_all()?;

        let (installed_runtime, _) = read_regular(destination)?;
        let (installed_provenance, _) = read_regular(&provenance_path)?;
        if installed_runtime != self.runtime || installed_provenance != self.provenance {
            return Err(invalid(
                "installed LiteInst pair differs from validated bytes",
            ));
        }
        validate(&installed_runtime, &installed_provenance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_validator(runtime: &[u8], provenance: &[u8]) -> io::Result<()> {
        let expected = match runtime {
            b"runtime-a" => b"provenance-a".as_slice(),
            b"runtime-b" => b"provenance-b".as_slice(),
            _ => return Err(invalid("unknown test runtime")),
        };
        if provenance != expected {
            return Err(invalid("mixed test pair"));
        }
        Ok(())
    }

    fn write_pair(root: &Path, name: &str, runtime: &[u8], provenance: &[u8]) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, runtime).unwrap();
        fs::write(provenance_path(&path), provenance).unwrap();
        path
    }

    #[test]
    fn installs_one_validated_snapshot_exactly() {
        let root = tempfile::tempdir().unwrap();
        let source = write_pair(root.path(), "source", b"runtime-a", b"provenance-a");
        let destination = root.path().join("installed");
        RuntimePair::read(&source, fixture_validator)
            .unwrap()
            .install(&destination, fixture_validator)
            .unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"runtime-a");
        assert_eq!(
            fs::read(root.path().join("installed.provenance.json")).unwrap(),
            b"provenance-a"
        );
    }

    #[test]
    fn mixed_pair_is_refused_before_installation() {
        let root = tempfile::tempdir().unwrap();
        let source = write_pair(root.path(), "source", b"runtime-a", b"provenance-b");
        let destination = root.path().join("installed");
        assert!(RuntimePair::read(&source, fixture_validator).is_err());
        assert!(!destination.exists());
        assert!(!provenance_path(&destination).exists());
    }

    #[test]
    fn interrupted_second_file_cannot_report_installation_success() {
        let root = tempfile::tempdir().unwrap();
        let source = write_pair(root.path(), "source", b"runtime-a", b"provenance-a");
        let destination = root.path().join("installed");
        fs::create_dir(provenance_path(&destination)).unwrap();
        let result = RuntimePair::read(&source, fixture_validator)
            .unwrap()
            .install(&destination, fixture_validator);
        assert!(result.is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"runtime-a");
        assert!(RuntimePair::read(&destination, fixture_validator).is_err());
    }
}
