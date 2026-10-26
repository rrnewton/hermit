//! Physical bundle finalization shared by the Cargo and native action adapters.
//!
//! This module is crate-private. Its callers must obtain fresh source/entry
//! authority before supplying the referenced evidence; serialized observations
//! do not authorize a runtime or an installation.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

#[path = "protected_copy.rs"]
mod protected_copy;
pub(crate) use protected_copy::copy_into_protected_stage;

/// Remove only this invocation's files if bundle publication fails.
/// Cargo can retain its existing replacement semantics. Native actions opt
/// into atomic no-replace publication, so a competing producer's output is
/// refused and never enrolled in this guard's cleanup. This remains failed-build
/// cleanup, not a transaction spanning concurrent producers.
pub(crate) struct NativeBundlePublication {
    directory: PathBuf,
    directory_file: fs::File,
    published: Vec<(PathBuf, fs::File)>,
    complete: bool,
    no_replace: bool,
}

impl NativeBundlePublication {
    pub(crate) fn create(directory: &Path, no_replace: bool) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;

        fs::create_dir(directory).map_err(|error| format!("create bundle stage: {error}"))?;
        let directory_file =
            fs::File::open(directory).map_err(|error| format!("open bundle stage: {error}"))?;
        let publication = Self {
            directory: directory.to_owned(),
            directory_file,
            published: Vec::new(),
            complete: false,
            no_replace,
        };
        let metadata = fs::symlink_metadata(directory)
            .map_err(|error| format!("read bundle stage metadata: {error}"))?;
        let opened = publication
            .directory_file
            .metadata()
            .map_err(|error| format!("read opened bundle stage metadata: {error}"))?;
        if !metadata.is_dir() || (metadata.dev(), metadata.ino()) != (opened.dev(), opened.ino()) {
            return Err("bundle stage is not the opened directory".into());
        }
        Ok(publication)
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn rename(&mut self, name: &str) -> Result<PathBuf, String> {
        use std::os::unix::fs::MetadataExt;

        let temporary = self.directory.join(name);
        let file = fs::File::open(&temporary)
            .map_err(|error| format!("open staged bundle file {name}: {error}"))?;
        let metadata = fs::symlink_metadata(&temporary)
            .map_err(|error| format!("read staged bundle file {name}: {error}"))?;
        let opened = file
            .metadata()
            .map_err(|error| format!("read opened bundle file {name}: {error}"))?;
        if !metadata.is_file() || (metadata.dev(), metadata.ino()) != (opened.dev(), opened.ino()) {
            return Err(format!(
                "staged bundle file {name} is not the opened regular file"
            ));
        }
        let published = self
            .directory
            .parent()
            .ok_or("bundle stage has no output parent")?
            .join(name);
        if self.no_replace {
            // On the same filesystem, link creates the output atomically only
            // if that name is still absent. A freshness check before the build
            // cannot provide this property. Existing files/symlinks are refused.
            fs::hard_link(&temporary, &published)
                .map_err(|error| format!("publish fresh bundle file {name}: {error}"))?;
        } else {
            fs::rename(&temporary, &published)
                .map_err(|error| format!("publish bundle file {name}: {error}"))?;
        }
        // Register before any published-byte readback or staging unlink can
        // fail. Retain the open file so unlink cannot recycle its inode during
        // cleanup. Failed hard_link never enrolls a competing producer's file.
        self.published.push((published.clone(), file));
        if self.no_replace {
            for path in [&temporary, &published] {
                let metadata = fs::symlink_metadata(path)
                    .map_err(|error| format!("read fresh bundle file {name}: {error}"))?;
                if !metadata.is_file()
                    || (metadata.dev(), metadata.ino()) != (opened.dev(), opened.ino())
                {
                    return Err(format!("fresh bundle file {name} changed identity"));
                }
            }
            fs::remove_file(&temporary)
                .map_err(|error| format!("remove linked bundle stage file {name}: {error}"))?;
        }
        Ok(published)
    }

    /// Native ordinary installs remove their empty stage from the closed
    /// resource directory. Cargo and native exec keep compilation intermediates.
    /// Any removal failure leaves this guard armed for owned-output rollback.
    pub(crate) fn complete(mut self, remove_empty_stage: bool) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;

        if remove_empty_stage {
            let metadata = fs::symlink_metadata(&self.directory)
                .map_err(|error| format!("read completing bundle stage: {error}"))?;
            let owned = self
                .directory_file
                .metadata()
                .map_err(|error| format!("read owned completing bundle stage: {error}"))?;
            if !metadata.is_dir() || (metadata.dev(), metadata.ino()) != (owned.dev(), owned.ino())
            {
                return Err("completing bundle stage is not the owned directory".into());
            }
            fs::remove_dir(&self.directory)
                .map_err(|error| format!("remove empty bundle stage: {error}"))?;
        }
        self.complete = true;
        Ok(())
    }
}

impl Drop for NativeBundlePublication {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;

        if self.complete {
            return;
        }
        for (path, file) in self.published.iter().rev() {
            if matches!(
                (fs::symlink_metadata(path), file.metadata()),
                (Ok(metadata), Ok(owned))
                    if metadata.is_file()
                        && (metadata.dev(), metadata.ino()) == (owned.dev(), owned.ino())
            ) {
                let _ = fs::remove_file(path);
            }
        }
        if matches!(
            (
                fs::symlink_metadata(&self.directory),
                self.directory_file.metadata(),
            ),
            (Ok(metadata), Ok(owned))
                if metadata.is_dir()
                    && (metadata.dev(), metadata.ino()) == (owned.dev(), owned.ino())
        ) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

#[cfg(feature = "native-exec")]
pub(crate) mod physical {
    use goblin::elf::Elf;
    use goblin::elf::program_header;
    use reverie_liteinst_artifact as qualifier;
    use sha2::Digest;
    use sha2::Sha256;

    use super::*;

    const GATE: u64 = 0x5000_0000_0000;

    fn hex_array(hex: &str) -> Result<[u8; 32], String> {
        if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("native bundle digest is not 64 hexadecimal bytes".into());
        }
        let mut digest = [0; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                .map_err(|error| format!("parse native bundle digest: {error}"))?;
        }
        Ok(digest)
    }

    // Refuse the same five dynamic tags as native_exec_artifacts before
    // publishing any newly compiled runner/runtime bytes. Never edit an ELF
    // to remove a forbidden tag after compilation.
    fn require_no_loader_overrides(path: &Path) -> Result<(), String> {
        use goblin::elf::dynamic;

        let bytes =
            fs::read(path).map_err(|error| format!("read produced native role: {error}"))?;
        let elf =
            Elf::parse(&bytes).map_err(|error| format!("parse produced native role: {error}"))?;
        if elf.dynamic.as_ref().is_some_and(|table| {
            table.dyns.iter().any(|d| {
                matches!(
                    d.d_tag,
                    dynamic::DT_RPATH
                        | dynamic::DT_RUNPATH
                        | dynamic::DT_TEXTREL
                        | dynamic::DT_AUDIT
                        | dynamic::DT_DEPAUDIT
                )
            })
        }) {
            return Err(format!(
                "produced native role requests unsupported loader search or text writes: {}",
                path.display()
            ));
        }
        Ok(())
    }

    fn gate_digest(path: &Path) -> Result<[u8; 32], String> {
        let bytes =
            fs::read(path).map_err(|error| format!("read current native bootstrap: {error}"))?;
        let elf = Elf::parse(&bytes)
            .map_err(|error| format!("parse current native bootstrap: {error}"))?;
        let segments: Vec<_> = elf
            .program_headers
            .iter()
            .filter(|p| {
                p.p_type == program_header::PT_LOAD
                    && GATE >= p.p_vaddr
                    && GATE.checked_add(4096).is_some_and(|end| {
                        p.p_vaddr
                            .checked_add(p.p_filesz)
                            .is_some_and(|hi| end <= hi)
                    })
            })
            .collect();
        if segments.len() != 1 {
            return Err("gate page not uniquely file backed".into());
        }
        let segment = segments[0];
        if segment.p_flags != (program_header::PF_R | program_header::PF_X) {
            return Err("native gate page permissions are not exactly read/execute".into());
        }
        let offset = segment
            .p_offset
            .checked_add(GATE - segment.p_vaddr)
            .ok_or("native gate page file offset overflows")?;
        let offset = usize::try_from(offset)
            .map_err(|error| format!("native gate page file offset: {error}"))?;
        let end = offset
            .checked_add(4096)
            .ok_or("native gate page extent overflows")?;
        Ok(Sha256::digest(bytes.get(offset..end).ok_or("truncated native gate page")?).into())
    }

    fn publish_file(
        name: &str,
        identity: &qualifier::FileIdentity,
        publication: &mut NativeBundlePublication,
    ) -> Result<(), String> {
        identity.verify()?;
        let temporary = publication.directory().join(name);
        copy_into_protected_stage(&identity.path, &temporary)
            .map_err(|error| format!("copy native bundle file {name}: {error}"))?;
        let copied = qualifier::FileIdentity::read(&temporary)?;
        if copied.sha256 != identity.sha256 {
            return Err("native bundle copy differs".into());
        }
        identity.verify()?;
        let published = publication.rename(name)?;
        if qualifier::FileIdentity::read(&published)?.sha256 != identity.sha256 {
            return Err("published native bundle copy differs".into());
        }
        Ok(())
    }

    fn publish_digest(
        name: &str,
        digest: &[u8; 32],
        publication: &mut NativeBundlePublication,
    ) -> Result<(), String> {
        let temporary = publication.directory().join(name);
        qualifier::write_new(&temporary, digest)?;
        if fs::read(&temporary)
            .map_err(|error| format!("read staged digest {name}: {error}"))?
            .as_slice()
            != digest
        {
            return Err(format!("staged native bundle digest {name} differs"));
        }
        let published = publication.rename(name)?;
        qualifier::real_file(&published)?;
        if fs::read(&published)
            .map_err(|error| format!("read published digest {name}: {error}"))?
            .as_slice()
            != digest
        {
            return Err(format!("published native bundle digest {name} differs"));
        }
        Ok(())
    }

    pub(crate) struct NativeBundle {
        bootstrap: qualifier::FileIdentity,
        runner: qualifier::FileIdentity,
        runtime: qualifier::FileIdentity,
        source_digest: [u8; 32],
        gate_digest: [u8; 32],
    }

    fn checked_runtime_identity(
        runtime: &Path,
        artifact: &qualifier::FileIdentity,
        close_sources: impl FnOnce() -> Result<(), String>,
    ) -> Result<qualifier::FileIdentity, String> {
        require_no_loader_overrides(runtime)?;
        close_sources()?;
        let runtime_identity = qualifier::FileIdentity::read(runtime)?;
        if runtime_identity.sha256 != artifact.sha256 {
            return Err("native bundle runtime differs from the qualified artifact".into());
        }
        artifact.verify()?;
        Ok(runtime_identity)
    }

    /// Shared physical checks after the caller has qualified the runtime.
    /// Evidence must come directly from the live Cargo qualifier or the opaque
    /// native-entry scope, never a deserialized record. The caller closes its
    /// sources after loader checks and before the artifact/gate join, and keeps
    /// the publication guard armed until its enclosing authority has closed.
    // Cargo's helper-only test harness cannot manufacture a live runtime
    // qualification. Keep this code typechecked there; the actual native entry
    // calls it in its Buck test harness and production action.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn validate_bundle(
        bootstrap: &Path,
        runner_file: &Path,
        runtime: &Path,
        artifact: &qualifier::FileIdentity,
        generic_bootstrap: &qualifier::GenericBootstrapEvidence,
        source_digest: [u8; 32],
        close_sources: impl FnOnce() -> Result<(), String>,
    ) -> Result<NativeBundle, String> {
        require_no_loader_overrides(runner_file)?;
        let runtime_identity = checked_runtime_identity(runtime, artifact, close_sources)?;
        let bootstrap_identity = qualifier::FileIdentity::read(bootstrap)?;
        let runner_identity = qualifier::FileIdentity::read(runner_file)?;
        if source_digest == [0; 32] {
            return Err("native bundle source digest is zero".into());
        }
        let gate_digest = gate_digest(bootstrap)?;
        // Join the actual compiled bootstrap page to the complete template
        // authenticated in the actual runtime. A matching entry address alone
        // does not bind the other bytes that runtime adoption will compare.
        generic_bootstrap
            .fixed_native_gate
            .as_ref()
            .ok_or("native runtime lacks qualified fixed gate evidence")?
            .verify_bootstrap_page_digest(&gate_digest)
            .map_err(|error| format!("native bootstrap/runtime gate pages differ: {error}"))?;
        Ok(NativeBundle {
            bootstrap: bootstrap_identity,
            runner: runner_identity,
            runtime: runtime_identity,
            source_digest,
            gate_digest,
        })
    }

    impl NativeBundle {
        pub(crate) fn bootstrap(&self) -> &qualifier::FileIdentity {
            &self.bootstrap
        }
        pub(crate) fn runner(&self) -> &qualifier::FileIdentity {
            &self.runner
        }
        pub(crate) fn runtime(&self) -> &qualifier::FileIdentity {
            &self.runtime
        }
        pub(crate) fn source_digest(&self) -> &[u8; 32] {
            &self.source_digest
        }
        pub(crate) fn gate_digest(&self) -> &[u8; 32] {
            &self.gate_digest
        }

        pub(crate) fn publish(
            &self,
            publication: &mut NativeBundlePublication,
        ) -> Result<(), String> {
            // Publish only data, after all existing source/entry/gate qualifications.
            // Each temporary is create-new inside this invocation's unique directory.
            for (name, identity) in [
                ("bootstrap", self.bootstrap()),
                ("runner", self.runner()),
                ("runtime", self.runtime()),
            ] {
                publish_file(&format!("native_exec_{name}.bin"), identity, publication)?;
                publish_digest(
                    &format!("native_exec_{name}.sha256"),
                    &hex_array(&identity.sha256)?,
                    publication,
                )?;
            }
            publish_digest(
                "native_exec_source.sha256",
                self.source_digest(),
                publication,
            )?;
            publish_digest("native_exec_gate.sha256", self.gate_digest(), publication)
        }
    }

    #[cfg(test)]
    mod tests {
        use goblin::elf::dynamic;

        use super::super::tests::BundleDirectory;
        use super::*;

        fn elf_header(programs: u16, size: usize) -> Vec<u8> {
            let mut bytes = vec![0; size];
            bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
            bytes[16..18].copy_from_slice(&3_u16.to_le_bytes());
            bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
            bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
            bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
            bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
            bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
            bytes[56..58].copy_from_slice(&programs.to_le_bytes());
            bytes
        }

        fn gate_elf() -> Vec<u8> {
            let mut bytes = elf_header(1, 8192);
            bytes[64..68].copy_from_slice(&program_header::PT_LOAD.to_le_bytes());
            bytes[68..72]
                .copy_from_slice(&(program_header::PF_R | program_header::PF_X).to_le_bytes());
            bytes[72..80].copy_from_slice(&4096_u64.to_le_bytes());
            bytes[80..88].copy_from_slice(&GATE.to_le_bytes());
            bytes[96..104].copy_from_slice(&4096_u64.to_le_bytes());
            bytes[104..112].copy_from_slice(&4096_u64.to_le_bytes());
            bytes[112..120].copy_from_slice(&4096_u64.to_le_bytes());
            bytes[4096..].fill(0xa5);
            bytes
        }

        #[test]
        fn native_gate_hashes_the_complete_page_and_refuses_invalid_extents() {
            let root = BundleDirectory::create("gate");
            let path = root.0.join("bootstrap");
            let original = gate_elf();
            fs::write(&path, &original).unwrap();
            let expected: [u8; 32] = Sha256::digest(&original[4096..8192]).into();
            assert_eq!(gate_digest(&path).unwrap(), expected);
            for offset in [4096, 4096 + 2048, 8191] {
                let mut changed = original.clone();
                changed[offset] ^= 1;
                fs::write(&path, &changed).unwrap();
                assert_ne!(gate_digest(&path).unwrap(), expected);
            }
            let mut flags = original.clone();
            flags[68..72].copy_from_slice(&program_header::PF_R.to_le_bytes());
            fs::write(&path, flags).unwrap();
            assert!(gate_digest(&path).unwrap_err().contains("permissions"));
            fs::write(&path, &original[..8191]).unwrap();
            assert!(gate_digest(&path).unwrap_err().contains("truncated"));
            let mut duplicate = original.clone();
            duplicate[56..58].copy_from_slice(&2_u16.to_le_bytes());
            duplicate.copy_within(64..120, 120);
            fs::write(&path, duplicate).unwrap();
            assert!(gate_digest(&path).unwrap_err().contains("uniquely"));
            let mut short = original;
            short[96..104].copy_from_slice(&4095_u64.to_le_bytes());
            fs::write(&path, short).unwrap();
            assert!(gate_digest(&path).unwrap_err().contains("uniquely"));
        }

        #[test]
        fn native_loader_refuses_each_original_override_tag() {
            let root = BundleDirectory::create("loader-tags");
            let path = root.0.join("role");
            let mut bytes = elf_header(1, 152);
            bytes[64..68].copy_from_slice(&program_header::PT_DYNAMIC.to_le_bytes());
            bytes[72..80].copy_from_slice(&120_u64.to_le_bytes());
            bytes[96..104].copy_from_slice(&32_u64.to_le_bytes());
            bytes[104..112].copy_from_slice(&32_u64.to_le_bytes());
            fs::write(&path, &bytes).unwrap();
            require_no_loader_overrides(&path).unwrap();
            for tag in [
                dynamic::DT_RPATH,
                dynamic::DT_RUNPATH,
                dynamic::DT_TEXTREL,
                dynamic::DT_AUDIT,
                dynamic::DT_DEPAUDIT,
            ] {
                bytes[120..128].copy_from_slice(&tag.to_le_bytes());
                fs::write(&path, &bytes).unwrap();
                assert!(
                    require_no_loader_overrides(&path)
                        .unwrap_err()
                        .contains("unsupported loader")
                );
            }
        }

        #[test]
        fn native_runtime_identity_requires_source_close_and_current_equal_bytes() {
            use std::cell::Cell;

            let root = BundleDirectory::create("runtime-identity");
            let original = root.0.join("original");
            let runtime = root.0.join("runtime");
            let bytes = gate_elf();
            fs::write(&original, &bytes).unwrap();
            fs::write(&runtime, &bytes).unwrap();
            let identity = qualifier::FileIdentity::read(&original).unwrap();
            let closed = Cell::new(false);
            checked_runtime_identity(&runtime, &identity, || {
                closed.set(true);
                Ok(())
            })
            .unwrap();
            assert!(closed.get());
            assert_eq!(
                checked_runtime_identity(&runtime, &identity, || {
                    Err("source custody changed".into())
                })
                .unwrap_err(),
                "source custody changed"
            );
            let mut changed = bytes;
            changed[8191] ^= 1;
            fs::write(&runtime, &changed).unwrap();
            assert!(
                checked_runtime_identity(&runtime, &identity, || Ok(()))
                    .unwrap_err()
                    .contains("differs from the qualified artifact")
            );
            fs::write(&runtime, fs::read(&original).unwrap()).unwrap();
            let error = checked_runtime_identity(&runtime, &identity, || {
                fs::write(&original, &changed).unwrap();
                Ok(())
            })
            .unwrap_err();
            assert!(error.contains("file identity changed"), "{error}");
        }

        #[test]
        fn native_publication_writes_exactly_eight_files_with_raw32_digests() {
            // This tests the publication helper only, not runtime qualification.
            let root = BundleDirectory::create("eight-file-publication");
            let inputs = root.0.join("inputs");
            fs::create_dir(&inputs).unwrap();
            for (name, bytes) in [
                ("bootstrap", b"checked bootstrap".as_slice()),
                ("runner", b"checked runner".as_slice()),
                ("runtime", b"checked runtime".as_slice()),
            ] {
                fs::write(inputs.join(name), bytes).unwrap();
            }
            let bundle = NativeBundle {
                bootstrap: qualifier::FileIdentity::read(&inputs.join("bootstrap")).unwrap(),
                runner: qualifier::FileIdentity::read(&inputs.join("runner")).unwrap(),
                runtime: qualifier::FileIdentity::read(&inputs.join("runtime")).unwrap(),
                source_digest: [7; 32],
                gate_digest: [11; 32],
            };
            let destination = root.0.join("stage");
            let mut publication = NativeBundlePublication::create(&destination, false).unwrap();
            bundle.publish(&mut publication).unwrap();
            let mut output_names = fs::read_dir(&root.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .filter(|name| name.starts_with("native_exec_"))
                .collect::<Vec<_>>();
            output_names.sort();
            assert_eq!(
                output_names,
                [
                    "native_exec_bootstrap.bin",
                    "native_exec_bootstrap.sha256",
                    "native_exec_gate.sha256",
                    "native_exec_runner.bin",
                    "native_exec_runner.sha256",
                    "native_exec_runtime.bin",
                    "native_exec_runtime.sha256",
                    "native_exec_source.sha256",
                ]
            );
            for (name, identity) in [
                ("bootstrap", &bundle.bootstrap),
                ("runner", &bundle.runner),
                ("runtime", &bundle.runtime),
            ] {
                assert_eq!(
                    fs::read(root.0.join(format!("native_exec_{name}.bin"))).unwrap(),
                    fs::read(&identity.path).unwrap()
                );
                assert_eq!(
                    fs::read(root.0.join(format!("native_exec_{name}.sha256"))).unwrap(),
                    hex_array(&identity.sha256).unwrap()
                );
            }
            assert_eq!(
                fs::read(root.0.join("native_exec_source.sha256")).unwrap(),
                [7; 32]
            );
            assert_eq!(
                fs::read(root.0.join("native_exec_gate.sha256")).unwrap(),
                [11; 32]
            );
            publication.complete(false).unwrap();
            assert!(root.0.join("native_exec_runtime.bin").is_file());
        }

        #[test]
        fn native_publication_requires_current_bytes_and_writes_raw_digest() {
            let root = BundleDirectory::create("physical-publish");
            let source = root.0.join("source");
            fs::write(&source, b"checked physical bytes").unwrap();
            let identity = qualifier::FileIdentity::read(&source).unwrap();
            let digest = hex_array(&identity.sha256).unwrap();
            let destination = root.0.join("stage");
            let mut publication = NativeBundlePublication::create(&destination, false).unwrap();
            publish_file("native_exec_runtime.bin", &identity, &mut publication).unwrap();
            publish_digest("native_exec_runtime.sha256", &digest, &mut publication).unwrap();
            assert_eq!(
                fs::read(root.0.join("native_exec_runtime.bin")).unwrap(),
                b"checked physical bytes"
            );
            assert_eq!(
                fs::read(root.0.join("native_exec_runtime.sha256")).unwrap(),
                digest
            );
            fs::write(&source, b"changed source").unwrap();
            assert!(publish_file("native_exec_runner.bin", &identity, &mut publication).is_err());
            drop(publication);
            assert!(!destination.exists());
            assert!(!root.0.join("native_exec_runtime.bin").exists());
            assert!(!root.0.join("native_exec_runtime.sha256").exists());
            assert!(!root.0.join("native_exec_runner.bin").exists());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    pub(super) struct BundleDirectory(pub(super) PathBuf);

    impl BundleDirectory {
        pub(super) fn create(label: &str) -> Self {
            let serial = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "hermit-native-bundle-{label}-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for BundleDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn native_bundle_failure_removes_partial_publication_and_owned_stage() {
        let root = BundleDirectory::create("failure");
        let destination = root.0.join("stage");
        fs::write(root.0.join("untouched"), b"previous output").unwrap();
        let failed = std::panic::catch_unwind(|| {
            let mut publication = NativeBundlePublication::create(&destination, false).unwrap();
            fs::write(destination.join("intermediate"), b"incomplete link").unwrap();
            for name in ["native_exec_bootstrap.bin", "native_exec_bootstrap.sha256"] {
                fs::write(destination.join(name), b"new output").unwrap();
                publication.rename(name).unwrap();
            }
            panic!("later publication failed");
        });
        assert!(failed.is_err());
        assert!(!destination.exists());
        assert!(!root.0.join("native_exec_bootstrap.bin").exists());
        assert!(!root.0.join("native_exec_bootstrap.sha256").exists());
        assert_eq!(
            fs::read(root.0.join("untouched")).unwrap(),
            b"previous output"
        );
    }

    #[test]
    fn native_bundle_completion_retains_outputs_and_intermediates() {
        let root = BundleDirectory::create("complete");
        let destination = root.0.join("stage");
        let mut publication = NativeBundlePublication::create(&destination, false).unwrap();
        assert_eq!(publication.directory(), destination.as_path());
        fs::write(destination.join("intermediate"), b"link evidence").unwrap();
        fs::write(destination.join("native_exec_runner.bin"), b"runner").unwrap();
        publication.rename("native_exec_runner.bin").unwrap();
        publication.complete(false).unwrap();
        assert_eq!(
            fs::read(root.0.join("native_exec_runner.bin")).unwrap(),
            b"runner"
        );
        assert_eq!(
            fs::read(destination.join("intermediate")).unwrap(),
            b"link evidence"
        );
    }

    #[test]
    fn native_bundle_cleanup_preserves_replaced_files_and_directories() {
        let root = BundleDirectory::create("replacement");
        let destination = root.0.join("stage");
        let mut publication = NativeBundlePublication::create(&destination, false).unwrap();
        fs::write(destination.join("native_exec_runner.bin"), b"runner").unwrap();
        publication.rename("native_exec_runner.bin").unwrap();
        // Allocate the replacement while the original inode still exists.
        fs::write(root.0.join("replacement"), b"replacement output").unwrap();
        fs::rename(
            root.0.join("replacement"),
            root.0.join("native_exec_runner.bin"),
        )
        .unwrap();
        fs::rename(&destination, root.0.join("moved-stage")).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("replacement"), b"replacement directory").unwrap();
        drop(publication);
        assert_eq!(
            fs::read(root.0.join("native_exec_runner.bin")).unwrap(),
            b"replacement output"
        );
        assert_eq!(
            fs::read(destination.join("replacement")).unwrap(),
            b"replacement directory"
        );
        assert!(root.0.join("moved-stage").is_dir());
        assert!(NativeBundlePublication::create(&destination, false).is_err());
        assert!(destination.join("replacement").is_file());
    }

    #[test]
    fn native_bundle_cleanup_retains_unlinked_identities_until_drop() {
        use std::os::unix::fs::MetadataExt;

        let root = BundleDirectory::create("unlinked");
        let destination = root.0.join("stage");
        let mut publication = NativeBundlePublication::create(&destination, false).unwrap();
        fs::write(destination.join("native_exec_runner.bin"), b"runner").unwrap();
        publication.rename("native_exec_runner.bin").unwrap();
        fs::remove_file(root.0.join("native_exec_runner.bin")).unwrap();
        fs::remove_dir(&destination).unwrap();
        // The owned descriptors keep these unlinked inodes from being reused
        // for replacements before cleanup checks their device/inode identity.
        assert_eq!(publication.published[0].1.metadata().unwrap().nlink(), 0);
        assert_eq!(publication.directory_file.metadata().unwrap().nlink(), 0);
        fs::write(root.0.join("native_exec_runner.bin"), b"replacement output").unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("replacement"), b"replacement directory").unwrap();
        drop(publication);
        assert_eq!(
            fs::read(root.0.join("native_exec_runner.bin")).unwrap(),
            b"replacement output"
        );
        assert_eq!(
            fs::read(destination.join("replacement")).unwrap(),
            b"replacement directory"
        );
    }

    #[test]
    fn native_bundle_result_failure_removes_partial_publication() {
        let root = BundleDirectory::create("result-failure");
        let destination = root.0.join("stage");
        let result = (|| -> Result<(), String> {
            let mut publication = NativeBundlePublication::create(&destination, false)?;
            fs::write(destination.join("native_exec_runtime.bin"), b"runtime").unwrap();
            publication.rename("native_exec_runtime.bin")?;
            // A missing later output is an expected refusal, not a panic. The
            // first renamed output and the stage still belong to this guard.
            publication.rename("native_exec_runtime.sha256")?;
            publication.complete(false).unwrap();
            Ok(())
        })();
        assert!(result.unwrap_err().contains("native_exec_runtime.sha256"));
        assert!(!destination.exists());
        assert!(!root.0.join("native_exec_runtime.bin").exists());
    }

    #[test]
    fn native_bundle_refuses_symlink_publication_without_removing_target() {
        use std::os::unix::fs::symlink;

        let root = BundleDirectory::create("symlink");
        let destination = root.0.join("stage");
        fs::write(root.0.join("unowned"), b"unowned target").unwrap();
        let mut publication = NativeBundlePublication::create(&destination, false).unwrap();
        symlink(
            root.0.join("unowned"),
            destination.join("native_exec_runner.bin"),
        )
        .unwrap();
        assert!(publication.rename("native_exec_runner.bin").is_err());
        drop(publication);
        assert!(!destination.exists());
        assert!(!root.0.join("native_exec_runner.bin").exists());
        assert_eq!(fs::read(root.0.join("unowned")).unwrap(), b"unowned target");
    }

    #[test]
    fn native_no_replace_refuses_late_output_and_rolls_back_only_owned_files() {
        use std::os::unix::fs::MetadataExt;

        let root = BundleDirectory::create("no-replace-late-output");
        let destination = root.0.join("stage");
        let mut publication = NativeBundlePublication::create(&destination, true).unwrap();
        fs::write(destination.join("runtime.so"), b"our runtime").unwrap();
        publication.rename("runtime.so").unwrap();
        // The other output appears after create and after our first publish.
        // No earlier freshness check could protect this publication boundary.
        let competing = root.0.join("receipt.json");
        fs::write(&competing, b"another producer's receipt").unwrap();
        let original = fs::metadata(&competing).unwrap();
        fs::write(destination.join("receipt.json"), b"our receipt").unwrap();
        assert!(
            publication
                .rename("receipt.json")
                .unwrap_err()
                .contains("publish fresh")
        );
        assert_eq!(fs::read(&competing).unwrap(), b"another producer's receipt");
        drop(publication);
        let retained = fs::metadata(&competing).unwrap();
        assert_eq!(
            (retained.dev(), retained.ino()),
            (original.dev(), original.ino())
        );
        assert_eq!(fs::read(&competing).unwrap(), b"another producer's receipt");
        assert!(!root.0.join("runtime.so").exists());
        assert!(!destination.exists());
    }

    #[test]
    fn native_no_replace_publishes_and_removes_only_its_empty_stage() {
        use std::os::unix::fs::MetadataExt;

        let root = BundleDirectory::create("no-replace-success");
        let destination = root.0.join("stage");
        let mut publication = NativeBundlePublication::create(&destination, true).unwrap();
        fs::write(destination.join("runtime.so"), b"runtime").unwrap();
        let original = fs::metadata(destination.join("runtime.so")).unwrap();
        let published = publication.rename("runtime.so").unwrap();
        let linked = fs::metadata(&published).unwrap();
        assert_eq!(
            (linked.dev(), linked.ino()),
            (original.dev(), original.ino())
        );
        assert_eq!(linked.nlink(), 1);
        assert!(!destination.join("runtime.so").exists());
        fs::write(destination.join("receipt.json"), b"receipt").unwrap();
        publication.rename("receipt.json").unwrap();
        publication.complete(true).unwrap();
        assert!(!destination.exists());
        assert_eq!(fs::read(published).unwrap(), b"runtime");
        assert_eq!(fs::read(root.0.join("receipt.json")).unwrap(), b"receipt");
    }

    #[test]
    fn native_completion_refuses_replaced_stage_and_preserves_it() {
        let root = BundleDirectory::create("complete-replaced-stage");
        let destination = root.0.join("stage");
        let mut publication = NativeBundlePublication::create(&destination, true).unwrap();
        fs::write(destination.join("runtime.so"), b"runtime").unwrap();
        publication.rename("runtime.so").unwrap();
        fs::rename(&destination, root.0.join("moved-stage")).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("unowned"), b"replacement directory").unwrap();
        assert!(
            publication
                .complete(true)
                .unwrap_err()
                .contains("not the owned directory")
        );
        assert_eq!(
            fs::read(destination.join("unowned")).unwrap(),
            b"replacement directory"
        );
        assert!(root.0.join("moved-stage").is_dir());
        assert!(!root.0.join("runtime.so").exists());
    }

    #[test]
    fn native_completion_failure_rolls_back_a_nonempty_owned_stage() {
        let root = BundleDirectory::create("complete-nonempty-stage");
        let destination = root.0.join("stage");
        let mut publication = NativeBundlePublication::create(&destination, true).unwrap();
        fs::write(destination.join("runtime.so"), b"runtime").unwrap();
        publication.rename("runtime.so").unwrap();
        fs::write(
            destination.join("unexpected-intermediate"),
            b"owned intermediate",
        )
        .unwrap();
        assert!(
            publication
                .complete(true)
                .unwrap_err()
                .contains("remove empty bundle stage")
        );
        assert!(!destination.exists());
        assert!(!root.0.join("runtime.so").exists());
    }

    #[test]
    fn cargo_replacement_mode_keeps_its_existing_output_semantics() {
        let root = BundleDirectory::create("cargo-replace");
        let destination = root.0.join("stage");
        fs::write(root.0.join("runtime.so"), b"old Cargo output").unwrap();
        let mut publication = NativeBundlePublication::create(&destination, false).unwrap();
        fs::write(destination.join("runtime.so"), b"new Cargo output").unwrap();
        publication.rename("runtime.so").unwrap();
        publication.complete(false).unwrap();
        assert_eq!(
            fs::read(root.0.join("runtime.so")).unwrap(),
            b"new Cargo output"
        );
        assert!(destination.is_dir());
    }

    #[test]
    fn protected_copy_retains_bytes_and_permissions_and_refuses_replacement() {
        use std::os::unix::fs::PermissionsExt;

        let root = BundleDirectory::create("copy");
        let source = root.0.join("source");
        let destination = root.0.join("destination");
        fs::write(&source, b"actual source").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).unwrap();
        copy_into_protected_stage(&source, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"actual source");
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o751
        );
        fs::write(&source, b"changed source").unwrap();
        assert_eq!(
            copy_into_protected_stage(&source, &destination)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&destination).unwrap(), b"actual source");
    }
}
