use super::*;

mod runtime_selection {
    use super::*;

    struct Directory(std::path::PathBuf);

    impl Directory {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "liteinst-selection-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn preload(directory: &Path) -> std::path::PathBuf {
        fs::create_dir_all(directory).unwrap();
        let path = directory.join(RUNTIME_NAME);
        let bytes = fixture();
        let record = serde_json::json!({
            "declared_reverie_rev": "1".repeat(40),
            "resolved_reverie_rev": "3".repeat(40),
            "source_kind": "local-diagnostic",
        });
        let marker = serde_json::to_vec(&provenance(&bytes, &"2".repeat(64), &record)).unwrap();
        stage_pair(
            &path,
            &bytes,
            &marker,
            &"1".repeat(40),
            &"2".repeat(64),
            true,
        )
        .unwrap();
        validate(&path).unwrap();
        path
    }

    fn validate(path: &Path) -> io::Result<()> {
        validate_file_identity(
            path,
            &"1".repeat(40),
            &"2".repeat(64),
            true,
            &"3".repeat(40),
        )
    }

    #[test]
    fn packaged_preload_without_private_resource_is_selected() {
        let root = Directory::new();
        let parent = root.path().join("bin");
        let resources = root.path().join("rsrcs");
        fs::create_dir(&parent).unwrap();
        let expected = preload(&resources);
        let selected =
            runtime_candidate_from(&parent, |name| Ok(Some(resources.join(name)))).unwrap();
        assert_eq!(selected, expected);
        validate(&selected).unwrap();
    }

    #[test]
    fn adjacent_and_deps_preload_with_resource_directory_are_selected() {
        for location in ["", "deps"] {
            let root = Directory::new();
            let parent = root.path().join("bin");
            let resources = root.path().join("rsrcs");
            fs::create_dir(&resources).unwrap();
            let expected = preload(&parent.join(location));
            let selected =
                runtime_candidate_from(&parent, |name| Ok(Some(resources.join(name)))).unwrap();
            assert_eq!(selected, expected);
            validate(&selected).unwrap();
        }
    }

    #[test]
    fn genuinely_absent_candidates_report_not_found() {
        let root = Directory::new();
        let resources = root.path().join("rsrcs");
        fs::create_dir(&resources).unwrap();
        let error =
            runtime_candidate_from(root.path(), |name| Ok(Some(resources.join(name)))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("was not staged"));
    }

    #[test]
    fn present_invalid_private_never_falls_back_to_valid_preload() {
        for location in ["bin", "bin/deps", "rsrcs"] {
            for kind in ["file", "directory", "dangling-symlink"] {
                let root = Directory::new();
                let parent = root.path().join("bin");
                let resources = root.path().join("rsrcs");
                preload(&parent);
                let directory = root.path().join(location);
                fs::create_dir_all(&directory).unwrap();
                let private = directory.join(private::RUNTIME_NAME);
                match kind {
                    "file" => fs::write(&private, b"invalid private ELF").unwrap(),
                    "directory" => fs::create_dir(&private).unwrap(),
                    "dangling-symlink" => {
                        std::os::unix::fs::symlink("missing-target", &private).unwrap()
                    }
                    _ => unreachable!(),
                }
                let selected =
                    runtime_candidate_from(&parent, |name| Ok(Some(resources.join(name)))).unwrap();
                assert_eq!(selected, private, "{location}/{kind}");
                assert!(validate(&selected).is_err(), "{location}/{kind}");
            }
        }
    }

    #[test]
    fn candidate_lookup_errors_never_fall_back() {
        for location in ["adjacent", "deps", "resource"] {
            let root = Directory::new();
            let parent = root.path().join("bin");
            let resources = root.path().join("rsrcs");
            preload(&resources);
            let bad = match location {
                "adjacent" => parent.clone(),
                "deps" => parent.join("deps"),
                "resource" => root.path().join("broken-resources"),
                _ => unreachable!(),
            };
            fs::create_dir_all(bad.parent().unwrap()).unwrap();
            fs::write(&bad, b"not a directory").unwrap();
            let error = runtime_candidate_from(&parent, |name| {
                Ok(Some(
                    if location == "resource" && name == private::RUNTIME_NAME {
                        bad.join(name)
                    } else {
                        resources.join(name)
                    },
                ))
            })
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NotADirectory, "{location}");
        }
    }

    #[test]
    fn resource_discovery_error_is_not_absence() {
        let root = Directory::new();
        preload(root.path());
        let error = runtime_candidate_from(root.path(), |_| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "resource discovery denied",
            ))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}

#[test]
fn source_inventory_records_tracked_deletion_instead_of_omitting_it() {
    let root =
        std::env::temp_dir().join(format!("liteinst-source-deletions-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "--quiet"]).unwrap();
    fs::write(root.join("input"), b"tracked").unwrap();
    git(&root, &["add", "--", "input"]).unwrap();
    let original = source_files(&root).unwrap();
    fs::remove_file(root.join("input")).unwrap();
    let deleted = source_files(&root).unwrap();
    assert_eq!(deleted["input"], serde_json::json!({"deleted":true}));
    assert_ne!(original, deleted);
    fs::write(root.join("input"), b"tracked").unwrap();
    assert_eq!(original, source_files(&root).unwrap());
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn dirty_hermit_is_local_diagnostic_only() {
    let pin = "1".repeat(40);
    let error = source_mode(false, &pin, &pin, b" M hermit-cli/src/lib.rs\n", b"")
        .expect_err("normal staging accepted dirty Hermit");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("clean Hermit"));

    let diagnostic = source_mode(true, &pin, &pin, b" M hermit-cli/src/lib.rs\n", b"")
        .expect("local-diagnostic staging rejected dirty Hermit");
    assert_eq!(diagnostic.kind, "local-diagnostic");
    assert!(diagnostic.hermit_dirty);
    assert!(!diagnostic.reverie_dirty);
}

fn put16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn fixture() -> Vec<u8> {
    let mut bytes = vec![0; 0x2000];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    put16(&mut bytes, 16, ET_DYN);
    put16(&mut bytes, 18, EM_X86_64);
    put32(&mut bytes, 20, 1);
    put64(&mut bytes, 32, 64);
    put16(&mut bytes, 52, 64);
    put16(&mut bytes, 54, 56);
    put16(&mut bytes, 56, 3);
    for (index, (kind, flags, address, size)) in [
        (PT_LOAD, PF_R | PF_W | PF_X, 0, 0x2000),
        (PT_DYNAMIC, PF_R, 0x200, 0xc0),
        (PT_GNU_RELRO, PF_R, 0, 0x1000),
    ]
    .into_iter()
    .enumerate()
    {
        let offset = 64 + index * 56;
        put32(&mut bytes, offset, kind);
        put32(&mut bytes, offset + 4, flags);
        put64(&mut bytes, offset + 8, address);
        put64(&mut bytes, offset + 16, address);
        put64(&mut bytes, offset + 32, size);
        put64(&mut bytes, offset + 40, size);
        put64(&mut bytes, offset + 48, 8);
    }
    for (index, (tag, value)) in [
        (DT_INIT_ARRAY, 0x400),
        (DT_INIT_ARRAYSZ, 8),
        (DT_SYMTAB, 0x580),
        (DT_SYMENT, 24),
        (DT_STRTAB, 0x500),
        (DT_STRSZ, 80),
        (DT_HASH, 0x620),
        (DT_RELA, 0x680),
        (DT_RELASZ, 48),
        (DT_RELAENT, 24),
        (DT_NULL, 0),
    ]
    .into_iter()
    .enumerate()
    {
        put64(&mut bytes, 0x200 + index * 16, tag);
        put64(&mut bytes, 0x208 + index * 16, value);
    }
    bytes[0x420..0x428].copy_from_slice(b"HLI_DSO1");
    put32(&mut bytes, 0x428, 1);
    put32(&mut bytes, 0x42c, 32);
    put32(&mut bytes, 0x430, 1);
    bytes[0x501..0x501 + DESCRIPTOR_NAME.len()].copy_from_slice(DESCRIPTOR_NAME.as_bytes());
    put32(&mut bytes, 0x598, 1);
    bytes[0x59c] = (STB_GLOBAL << 4) | STT_OBJECT;
    put16(&mut bytes, 0x59e, 1);
    put64(&mut bytes, 0x5a0, 0x420);
    put64(&mut bytes, 0x5a8, 32);
    bytes[0x5b4] = (STB_LOCAL << 4) | STT_FUNC;
    put16(&mut bytes, 0x5b6, 1);
    put64(&mut bytes, 0x5b8, 0x1900);
    put32(&mut bytes, 0x620, 1);
    put32(&mut bytes, 0x624, 3);
    put32(&mut bytes, 0x628, 1);
    for (index, address) in [0x400, 0x438].into_iter().enumerate() {
        let offset = 0x680 + index * 24;
        put64(&mut bytes, offset, address);
        put64(&mut bytes, offset + 8, R_X86_64_RELATIVE.into());
        put64(&mut bytes, offset + 16, 0x1900);
    }
    bytes[0x1900] = 0xc3;
    bytes
}

fn private_fixture() -> Vec<u8> {
    let mut bytes = fixture();
    put32(&mut bytes, 68, PF_R | PF_X);
    put64(&mut bytes, 24, 0x1800);
    put64(&mut bytes, 40, 0x900);
    put16(&mut bytes, 58, 64);
    put16(&mut bytes, 60, 3);
    put32(&mut bytes, 0x944, goblin::elf::section_header::SHT_SYMTAB);
    put64(&mut bytes, 0x958, 0xa00);
    put64(&mut bytes, 0x960, 4 * 24);
    put32(&mut bytes, 0x968, 2);
    put64(&mut bytes, 0x978, 24);
    put32(&mut bytes, 0x984, goblin::elf::section_header::SHT_STRTAB);
    put64(&mut bytes, 0x998, 0xb00);
    let mut strings = vec![0];
    for (index, (name, kind, address, size)) in [
        (DESCRIPTOR_NAME, STT_OBJECT, 0x420, 32),
        ("pe_kernel_entry", STT_FUNC, 0x1800, 8),
        ("pe_private_crt_entry", STT_FUNC, 0x1810, 8),
    ]
    .into_iter()
    .enumerate()
    {
        let slot = 0xa00 + (index + 1) * 24;
        put32(&mut bytes, slot, strings.len() as u32);
        strings.extend_from_slice(name.as_bytes());
        strings.push(0);
        bytes[slot + 4] = (STB_GLOBAL << 4) | kind;
        put16(&mut bytes, slot + 6, 1);
        put64(&mut bytes, slot + 8, address);
        put64(&mut bytes, slot + 16, size);
    }
    put64(&mut bytes, 0x9a0, strings.len() as u64);
    bytes[0xb00..0xb00 + strings.len()].copy_from_slice(&strings);
    bytes
}

#[test]
fn private_constructor_requires_distinct_static_kernel_and_crt() {
    let bytes = private_fixture();
    validate_private_runtime(&bytes).unwrap();
    for (offset, value) in [(24, 0), (24, 0x1810), (0xa50, 0x1804)] {
        let mut bad = bytes.clone();
        put64(&mut bad, offset, value);
        assert!(validate_private_runtime(&bad).is_err());
    }
    let mut writable = bytes.clone();
    put32(&mut writable, 68, PF_R | PF_W | PF_X);
    assert!(validate_private_runtime(&writable).is_err());
    let mut interpreter = bytes.clone();
    put32(&mut interpreter, 64 + 2 * 56, PT_INTERP);
    assert!(validate_private_runtime(&interpreter).is_err());
    let mut dependency = bytes;
    put64(&mut dependency, 0x200 + 10 * 16, DT_NEEDED);
    assert!(validate_private_runtime(&dependency).is_err());
}

#[test]
fn private_provenance_cannot_be_swapped_with_preload_or_normal_mode() {
    let bytes = private_fixture();
    let pin = "a".repeat(40);
    let source = "b".repeat(64);
    let record = serde_json::json!({"runtime_kind":"private-crt", "source_kind":"local-diagnostic", "declared_reverie_rev":pin, "resolved_reverie_rev":pin});
    let sidecar = serde_json::to_vec(&provenance(&bytes, &source, &record)).unwrap();
    validate_provenance(&bytes, &sidecar, &pin, &source, true).unwrap();
    assert!(validate_provenance(&bytes, &sidecar, &pin, &source, false).is_err());
    assert!(validate_provenance(&fixture(), &sidecar, &pin, &source, true).is_err());
    let mut malformed = provenance(&bytes, &source, &record);
    malformed["artifact"] = serde_json::json!("hermit-liteinst-detcore-runtime");
    assert!(
        validate_provenance(
            &bytes,
            &serde_json::to_vec(&malformed).unwrap(),
            &pin,
            &source,
            true
        )
        .is_err()
    );
}

#[test]
fn relocated_constructor_and_rela_ignores_old_slot() {
    let mut bytes = fixture();
    validate_constructor(&bytes).unwrap();
    put64(&mut bytes, 0x400, 0xdeadbeef);
    put64(&mut bytes, 0x438, 0xdeadbeef);
    validate_constructor(&bytes).unwrap();
}

#[test]
fn relro_requires_effectively_protected_mapped_pages() {
    validate_constructor(&fixture()).unwrap();
    for (address, size, file_offset) in [
        (0x400, 0x40, 0x400),
        (0, 0xfff, 0),
        (0, 0x1000, 8),
        (0, 0x3000, 0),
    ] {
        let mut bytes = fixture();
        let offset = 64 + 2 * 56;
        put64(&mut bytes, offset + 8, file_offset);
        put64(&mut bytes, offset + 16, address);
        put64(&mut bytes, offset + 32, size);
        put64(&mut bytes, offset + 40, size);
        assert!(validate_constructor(&bytes).is_err());
    }
    let mut bytes = fixture();
    put32(&mut bytes, 68, PF_R | PF_X);
    put32(&mut bytes, 64 + 2 * 56, PT_NULL);
    validate_constructor(&bytes).unwrap();
}

#[test]
fn duplicate_relro_headers_refuse_in_either_order() {
    for size in [0, 0x1000] {
        for reverse in [false, true] {
            let mut bytes = fixture();
            put16(&mut bytes, 56, 4);
            let first = 64 + 2 * 56;
            let second = 64 + 3 * 56;
            bytes.copy_within(first..first + 56, second);
            put64(&mut bytes, second + 8, 0x1000);
            put64(&mut bytes, second + 16, 0x1000);
            put64(&mut bytes, second + 32, size);
            put64(&mut bytes, second + 40, size);
            if reverse {
                let header = bytes[first..first + 56].to_vec();
                bytes.copy_within(second..second + 56, first);
                bytes[second..second + 56].copy_from_slice(&header);
            }
            for flags in [PF_R | PF_W | PF_X, PF_R | PF_X] {
                put32(&mut bytes, 68, flags);
                assert_eq!(
                    validate_constructor(&bytes).unwrap_err().to_string(),
                    "multiple PT_GNU_RELRO headers",
                );
            }
        }
    }
}

#[test]
fn partial_loads_bss_boundaries_and_page_aliases_refuse() {
    for (address, file_size, memory_size) in [
        (0x420, 8, 8),
        (0x430, 8, 8),
        (0x208, 8, 8),
        (0x688, 8, 8),
        (0x430, 0, 8),
        (0x100, 1, 1),
        (0xff8, 8, 16),
    ] {
        let mut bytes = fixture();
        put16(&mut bytes, 56, 4);
        let offset = 64 + 3 * 56;
        put32(&mut bytes, offset, PT_LOAD);
        put32(&mut bytes, offset + 4, PF_R | PF_W);
        put64(&mut bytes, offset + 8, address);
        put64(&mut bytes, offset + 16, address);
        put64(&mut bytes, offset + 32, file_size);
        put64(&mut bytes, offset + 40, memory_size);
        put64(&mut bytes, offset + 48, PAGE_SIZE);
        assert!(
            validate_constructor(&bytes)
                .unwrap_err()
                .to_string()
                .contains("overlapping load pages")
        );
    }
    let mut bytes = fixture();
    put64(&mut bytes, 96, 0x1f00);
    put64(&mut bytes, 104, 0x1f00);
    put16(&mut bytes, 56, 4);
    let offset = 64 + 3 * 56;
    put32(&mut bytes, offset, PT_LOAD);
    put32(&mut bytes, offset + 4, PF_R | PF_W);
    put64(&mut bytes, offset + 8, 0x1f80);
    put64(&mut bytes, offset + 16, 0x1f80);
    put64(&mut bytes, offset + 32, 8);
    put64(&mut bytes, offset + 40, 8);
    put64(&mut bytes, offset + 48, PAGE_SIZE);
    assert!(
        validate_constructor(&bytes)
            .unwrap_err()
            .to_string()
            .contains("overlapping load pages")
    );
    for (offset, value) in [(72, 1), (112, 3), (96, 0x2001), (96, 0x430), (104, 0x400)] {
        let mut bytes = fixture();
        put64(&mut bytes, offset, value);
        assert!(validate_constructor(&bytes).is_err());
    }
}

#[test]
fn actual_detcore_cache_requires_matching_complete_pair() {
    let directory =
        std::env::temp_dir().join(format!("liteinst-detcore-cache-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let runtime = directory.join(RUNTIME_NAME);
    let sidecar = directory.join(format!("{RUNTIME_NAME}.provenance.json"));
    let pin = "1".repeat(40);
    let source = "2".repeat(64);
    let resolved = "3".repeat(40);
    let matches = || validate_file_identity(&runtime, &pin, &source, true, &resolved).is_ok();
    assert!(!matches());
    let bytes = fixture();
    fs::write(&runtime, &bytes).unwrap();
    assert!(!matches());
    let record = serde_json::json!({"declared_reverie_rev": pin, "resolved_reverie_rev": resolved, "source_kind": "local-diagnostic"});
    let marker = serde_json::to_vec(&provenance(&bytes, &source, &record)).unwrap();
    fs::write(&sidecar, &marker).unwrap();
    assert!(matches());
    assert!(validate_snapshot_identity(&runtime, &bytes, &pin, &source, true, &resolved).is_ok());
    assert!(
        validate_snapshot_identity(
            &runtime,
            b"different snapshot",
            &pin,
            &source,
            true,
            &resolved
        )
        .is_err()
    );
    assert!(
        validate_snapshot_identity(&runtime, &bytes, &pin, &source, true, &"4".repeat(40)).is_err()
    );
    for (field, value) in [
        ("source_pair_sha256", "4".repeat(64)),
        ("resolved_reverie_rev", "4".repeat(40)),
        ("declared_reverie_rev", "4".repeat(40)),
    ] {
        let mut changed: Value = serde_json::from_slice(&marker).unwrap();
        changed[field] = Value::String(value);
        fs::write(&sidecar, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(!matches());
    }
    fs::write(&sidecar, &marker).unwrap();
    fs::write(&runtime, b"fixture").unwrap();
    assert!(!matches());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rel_uses_nonzero_implicit_addends() {
    let mut bytes = fixture();
    put64(&mut bytes, 0x270, DT_REL);
    put64(&mut bytes, 0x280, DT_RELSZ);
    put64(&mut bytes, 0x288, 32);
    put64(&mut bytes, 0x290, DT_RELENT);
    put64(&mut bytes, 0x298, 16);
    put64(&mut bytes, 0x690, 0x438);
    put64(&mut bytes, 0x698, R_X86_64_RELATIVE.into());
    put64(&mut bytes, 0x400, 0x1900);
    put64(&mut bytes, 0x438, 0x1900);
    validate_constructor(&bytes).unwrap();
    put64(&mut bytes, 0x438, 0);
    assert!(validate_constructor(&bytes).is_err());
}

#[test]
fn symbol_relocations_require_nonpreemptible_functions() {
    let mut bytes = fixture();
    for offset in [0x680, 0x698] {
        put64(
            &mut bytes,
            offset + 8,
            (2u64 << 32) | u64::from(R_X86_64_64),
        );
        put64(&mut bytes, offset + 16, 0);
    }
    validate_constructor(&bytes).unwrap();
    bytes[0x5b4] = (STB_GLOBAL << 4) | STT_FUNC;
    assert!(validate_constructor(&bytes).is_err());
    bytes[0x5b5] = STV_HIDDEN;
    validate_constructor(&bytes).unwrap();
    for visibility in [STV_DEFAULT, STV_PROTECTED] {
        bytes[0x5b5] = visibility;
        assert!(validate_constructor(&bytes).is_err());
    }
    bytes[0x5b5] = STV_HIDDEN;
    bytes[0x5b4] = (STB_WEAK << 4) | STT_FUNC;
    assert!(validate_constructor(&bytes).is_err());
}

#[test]
fn malformed_loader_contract_refuses() {
    for (offset, value) in [
        (0x200, DT_NULL),
        (0x208, 0xff8),
        (0x218, 0),
        (0x218, 7),
        (0x210, DT_INIT_ARRAY),
        (0x298, 16),
        (0x288, 25),
        (0x270, 36),
        (0x680, 0x408),
        (0x698, 0x440),
        (0x690, 0x1001),
        (0x6a8, 0x901),
        (0x6a0, u64::from(R_X86_64_JUMP_SLOT)),
        (0x438, 0x1900),
    ] {
        let mut bytes = fixture();
        put64(&mut bytes, offset, value);
        if offset == 0x438 {
            put64(&mut bytes, 0x288, 24);
        }
        assert!(
            validate_constructor(&bytes).is_err(),
            "accepted mutation {offset:x}={value:x}"
        );
    }
}

#[test]
fn conflicting_relocation_and_bare_array_refuse() {
    let mut bytes = fixture();
    put64(&mut bytes, 0x288, 72);
    put64(&mut bytes, 0x6b0, 0x437);
    put64(&mut bytes, 0x6b8, u64::from(R_X86_64_RELATIVE));
    put64(&mut bytes, 0x6c0, 0x1900);
    assert!(validate_constructor(&bytes).is_err());
    let mut bytes = fixture();
    put64(&mut bytes, 0x680, 0x408);
    put64(&mut bytes, 0x400, 0x1900);
    assert!(validate_constructor(&bytes).is_err());
}

#[test]
fn provenance_checks_source_mode_bytes_and_unknown_fields() {
    let bytes = fixture();
    let pin = "1".repeat(40);
    let source = "2".repeat(64);
    let record = serde_json::json!({"declared_reverie_rev": pin, "resolved_reverie_rev": "3".repeat(40), "source_kind": "local-diagnostic"});
    let sidecar = provenance(&bytes, &source, &record);
    let encoded = serde_json::to_vec(&sidecar).unwrap();
    validate_provenance(&bytes, &encoded, &pin, &source, true).unwrap();
    assert!(validate_provenance(&bytes, &encoded, &pin, &source, false).is_err());
    assert!(validate_provenance(&bytes, &encoded, "unknown", &source, true).is_err());
    assert!(validate_provenance(&bytes, &encoded, &pin, &"4".repeat(64), true).is_err());
    let mut changed = bytes.clone();
    changed[0x950] = 1;
    assert!(validate_provenance(&changed, &encoded, &pin, &source, true).is_err());
    let mut unknown = sidecar;
    unknown["extra"] = Value::Bool(true);
    assert!(
        validate_provenance(
            &bytes,
            &serde_json::to_vec(&unknown).unwrap(),
            &pin,
            &source,
            true
        )
        .is_err()
    );
}

#[test]
fn protected_stage_and_interrupted_replacement() {
    let directory =
        std::env::temp_dir().join(format!("liteinst-artifact-unit-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let destination = directory.join("runtime.so.1");
    let bytes = fixture();
    let pin = "1".repeat(40);
    let source = "2".repeat(64);
    let record = serde_json::json!({"declared_reverie_rev": pin, "resolved_reverie_rev": "3".repeat(40), "source_kind": "local-diagnostic"});
    let sidecar = serde_json::to_vec(&provenance(&bytes, &source, &record)).unwrap();
    stage_pair(&destination, &bytes, &sidecar, &pin, &source, true).unwrap();
    assert!(stage_pair(&destination, &bytes, &sidecar, &pin, &source, true).is_err());
    validate_file(&destination, &pin, &source, true).unwrap();
    validate_file_identity(&destination, &pin, &source, true, &"3".repeat(40)).unwrap();
    assert!(validate_file_identity(&destination, &pin, &source, true, &"4".repeat(40)).is_err());
    let mut changed = bytes.clone();
    changed[0x950] = 1;
    fs::write(&destination, changed).unwrap();
    assert!(validate_file(&destination, &pin, &source, true).is_err());
    let link = directory.join("link.so");
    std::os::unix::fs::symlink(&destination, &link).unwrap();
    assert!(stage_pair(&link, &bytes, &sidecar, &pin, &source, true).is_err());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn sections_cannot_override_dynamic_constructor_metadata() {
    let mut bytes = fixture();
    put64(&mut bytes, 40, 0xa00);
    put16(&mut bytes, 58, 64);
    put16(&mut bytes, 60, 2);
    put32(
        &mut bytes,
        0xa44,
        goblin::elf::section_header::SHT_INIT_ARRAY,
    );
    put64(&mut bytes, 0xa50, 0x400);
    put64(&mut bytes, 0xa58, 0x400);
    put64(&mut bytes, 0xa60, 8);
    validate_constructor(&bytes).unwrap();
    for (offset, value) in [
        (0x218, 16),
        (0xa58, 0x408),
        (0xa50, 0x408),
        (0x210, DT_NULL),
    ] {
        let mut changed = bytes.clone();
        put64(&mut changed, offset, value);
        assert!(validate_constructor(&changed).is_err());
    }
    let table = bytes[0x200..0x2c0].to_vec();
    bytes[0xb00..0xbc0].copy_from_slice(&table);
    put64(&mut bytes, 64 + 56 + 8, 0xb00);
    assert!(validate_constructor(&bytes).is_err());
}

#[test]
fn conflicting_plt_tables_and_non_executable_targets_refuse() {
    let mut bytes = fixture();
    put64(&mut bytes, 64 + 56 + 32, 0xe0);
    put64(&mut bytes, 64 + 56 + 40, 0xe0);
    for (index, (tag, value)) in [
        (DT_JMPREL, 0x680),
        (DT_PLTRELSZ, 48),
        (DT_PLTREL, DT_RELA),
        (DT_NULL, 0),
    ]
    .into_iter()
    .enumerate()
    {
        put64(&mut bytes, 0x2a0 + index * 16, tag);
        put64(&mut bytes, 0x2a8 + index * 16, value);
    }
    assert!(validate_constructor(&bytes).is_err());
    let mut bytes = fixture();
    put32(&mut bytes, 68, PF_R | PF_W);
    assert!(validate_constructor(&bytes).is_err());
}

#[test]
fn signed_rel_symbol_addend_and_undefined_function_refusal() {
    let mut bytes = fixture();
    put64(&mut bytes, 0x270, DT_REL);
    put64(&mut bytes, 0x280, DT_RELSZ);
    put64(&mut bytes, 0x288, 32);
    put64(&mut bytes, 0x290, DT_RELENT);
    put64(&mut bytes, 0x298, 16);
    put64(&mut bytes, 0x690, 0x438);
    for offset in [0x688, 0x698] {
        put64(&mut bytes, offset, (2u64 << 32) | u64::from(R_X86_64_64));
    }
    put64(&mut bytes, 0x5b8, 0x1901);
    put64(&mut bytes, 0x400, u64::MAX);
    put64(&mut bytes, 0x438, u64::MAX);
    validate_constructor(&bytes).unwrap();
    put16(&mut bytes, 0x5b6, 0);
    assert!(validate_constructor(&bytes).is_err());
    put16(&mut bytes, 0x5b6, 0xfff1);
    assert!(validate_constructor(&bytes).is_err());
    put16(&mut bytes, 0x5b6, 1);
    bytes[0x5b4] = STT_GNU_IFUNC;
    assert!(validate_constructor(&bytes).is_err());
}

#[test]
fn normal_identity_requires_normal_record_and_exact_pin() {
    let bytes = fixture();
    let pin = "1".repeat(40);
    let identity = "2".repeat(64);
    let record = serde_json::json!({"declared_reverie_rev": pin, "resolved_reverie_rev": pin, "source_kind":"pinned-git"});
    let encoded = serde_json::to_vec(&provenance(&bytes, &identity, &record)).unwrap();
    validate_provenance(&bytes, &encoded, &pin, &identity, false).unwrap();
    assert!(validate_provenance(&bytes, &encoded, &pin, &identity, true).is_err());
    assert!(validate_provenance(&bytes, &encoded, &"3".repeat(40), &identity, false).is_err());
}

#[test]
fn loader_relative_prefix_must_match_table_types() {
    let mut bytes = fixture();
    put64(&mut bytes, 0x2a0, DT_RELACOUNT);
    put64(&mut bytes, 0x2a8, 1);
    validate_constructor(&bytes).unwrap();
    let mut oversized = bytes.clone();
    put64(&mut oversized, 0x2a8, 3);
    assert!(validate_constructor(&oversized).is_err());
    put64(&mut bytes, 0x688, (2u64 << 32) | u64::from(R_X86_64_64));
    put64(&mut bytes, 0x690, 0);
    assert!(validate_constructor(&bytes).is_err());
}
