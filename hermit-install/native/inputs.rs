//! Native host inputs. These records are data until R freshly rebinds them.
use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use proof::native_action::DeclaredGraph;
use proof::native_action::ExpectedGraph;
use reverie_liteinst_artifact as proof;
use serde::Deserialize;
use serde::Deserializer;
use serde::de::DeserializeOwned;
use serde::de::Error;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde_json::Map;
use serde_json::Number;
use serde_json::Value;

const MAX_INPUT_BYTES: usize = 16 << 20;

pub(crate) fn require(ok: bool, message: &str) -> proof::Result<()> {
    if ok { Ok(()) } else { Err(message.into()) }
}

// Same strict recursive decoder as R's private native_wire module. Keep it here
// so the native entries need no additional source-copy module. Parsing remains
// data validation and cannot create source or physical qualification authority.
struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Unique;
        impl<'de> Visitor<'de> for Unique {
            type Value = UniqueValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("JSON with unique object keys")
            }

            fn visit_bool<E: Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Bool(value)))
            }

            fn visit_i64<E: Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Number(value.into())))
            }

            fn visit_u64<E: Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Number(value.into())))
            }

            fn visit_f64<E: Error>(self, value: f64) -> Result<Self::Value, E> {
                Number::from_f64(value)
                    .map(|number| UniqueValue(Value::Number(number)))
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }

            fn visit_str<E: Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(value.to_owned())))
            }

            fn visit_string<E: Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(value)))
            }

            fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<Self::Value, A::Error> {
                let mut result = Vec::new();
                while let Some(UniqueValue(value)) = values.next_element()? {
                    result.push(value);
                }
                Ok(UniqueValue(Value::Array(result)))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut values: A) -> Result<Self::Value, A::Error> {
                let mut result = Map::new();
                while let Some((key, UniqueValue(value))) =
                    values.next_entry::<String, UniqueValue>()?
                {
                    if result.insert(key.clone(), value).is_some() {
                        return Err(A::Error::custom(format!("duplicate JSON key: {key}")));
                    }
                }
                Ok(UniqueValue(Value::Object(result)))
            }
        }
        deserializer.deserialize_any(Unique)
    }
}

fn same_snapshot(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.is_file()
        && right.is_file()
        && left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.mode() == right.mode()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

fn read_snapshot(path: &Path) -> proof::Result<(proof::FileIdentity, Vec<u8>)> {
    let path = proof::real_file(path)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .map_err(|e| format!("open native input {}: {e}", path.display()))?;
    let before = file.metadata().map_err(|e| e.to_string())?;
    let named = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    require(
        same_snapshot(&before, &named),
        "native input is not the opened regular file",
    )?;
    require(
        before.len() <= MAX_INPUT_BYTES as u64,
        "native input exceeds 16 MiB byte bound",
    )?;
    let mut bytes = Vec::new();
    // The extra byte detects growth past the bound; never parse a truncated
    // prefix. O_NONBLOCK also prevents a raced FIFO replacement from hanging.
    file.by_ref()
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read native input {}: {e}", path.display()))?;
    require(
        bytes.len() <= MAX_INPUT_BYTES,
        "native input exceeds 16 MiB byte bound",
    )?;
    let after = file.metadata().map_err(|e| e.to_string())?;
    let named = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    require(
        same_snapshot(&before, &after) && same_snapshot(&after, &named),
        "native input changed while reading",
    )?;
    let identity = proof::FileIdentity {
        path,
        sha256: proof::sha256(&bytes),
    };
    Ok((identity, bytes))
}

fn verify_snapshot(identity: &proof::FileIdentity) -> proof::Result<()> {
    let (current, _) = read_snapshot(&identity.path)?;
    require(current == *identity, "native input identity changed")
}

fn parse_snapshot<T: DeserializeOwned>(
    identity: &proof::FileIdentity,
    bytes: &[u8],
) -> proof::Result<T> {
    require(
        bytes.len() <= MAX_INPUT_BYTES,
        "native input exceeds 16 MiB byte bound",
    )?;
    require(
        proof::sha256(bytes) == identity.sha256,
        "native input buffer digest differs from its file identity",
    )?;
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let UniqueValue(value) =
        UniqueValue::deserialize(&mut decoder).map_err(|e| format!("native input JSON: {e}"))?;
    decoder
        .end()
        .map_err(|e| format!("native input JSON: {e}"))?;
    serde_json::from_value(value).map_err(|e| format!("native input graph: {e}"))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> proof::Result<(proof::FileIdentity, T)> {
    let (identity, bytes) = read_snapshot(path)?;
    let value = parse_snapshot(&identity, &bytes)?;
    verify_snapshot(&identity)?;
    Ok((identity, value))
}

/// Compare one small source file to its compiled literal bytes. This uses the
/// 16 MiB snapshot bound; bootstrap and runner products use bundle_file instead.
#[cfg(feature = "native-exec")]
pub(crate) fn verify_source_file_bytes(
    identity: &proof::FileIdentity,
    expected: &[u8],
) -> proof::Result<()> {
    let (current, bytes) = read_snapshot(&identity.path)?;
    require(current == *identity, "native source file identity changed")?;
    require(
        bytes == expected,
        "native bootstrap source bytes differ from compiled digest inputs",
    )
}

pub(crate) struct Inputs {
    pub expected: ExpectedGraph,
    pub declared: DeclaredGraph,
    pub expected_file: proof::FileIdentity,
    pub declared_file: proof::FileIdentity,
}
impl Inputs {
    pub fn read(expected: &Path, declared: &Path) -> proof::Result<Self> {
        let (expected_file, expected) = read_json(expected)?;
        let (declared_file, declared) = read_json(declared)?;
        let value = Self {
            expected,
            declared,
            expected_file,
            declared_file,
        };
        value.verify()?;
        Ok(value)
    }
    pub fn verify(&self) -> proof::Result<()> {
        verify_snapshot(&self.expected_file)?;
        verify_snapshot(&self.declared_file)
    }
}

#[cfg(feature = "native-exec")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePin {
    sha256: String,
    size: u64,
}

#[cfg(feature = "native-exec")]
fn bundle_digest(reader: &mut impl Read, expected_size: u64) -> proof::Result<String> {
    use sha2::Digest;

    let bound = expected_size
        .checked_add(1)
        .ok_or("native bundle input size cannot be bounded")?;
    let mut reader = reader.take(bound);
    let mut hash = sha2::Sha256::new();
    let mut observed = 0_u64;
    let mut buffer = [0; 64 << 10];
    loop {
        let count = reader.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        observed += count as u64;
        hash.update(&buffer[..count]);
    }
    require(
        observed == expected_size,
        "native bundle input read size differs from expected",
    )?;
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(feature = "native-exec")]
fn bundle_file(path: &Path, pin: &FilePin) -> proof::Result<proof::FileIdentity> {
    let before = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    require(
        before.is_file(),
        "native bundle input must be a real regular file",
    )?;
    require(
        before.len() == pin.size,
        "native bundle input size differs from expected",
    )?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("open native bundle input {}: {e}", path.display()))?;
    let opened = file.metadata().map_err(|e| e.to_string())?;
    let canonical = proof::real_file(path)?;
    let named = fs::symlink_metadata(&canonical).map_err(|e| e.to_string())?;
    require(
        same_snapshot(&before, &opened) && same_snapshot(&opened, &named),
        "native bundle input is not the opened regular file",
    )?;
    // Hash this one no-follow descriptor, bounded by the selected pin's size
    // plus a sentinel. Product pins record stability after prerequisite actions;
    // source expectations are independently authored. A path reopen could block
    // on a raced FIFO.
    let sha256 = bundle_digest(&mut file, pin.size)?;
    let after = file.metadata().map_err(|e| e.to_string())?;
    let named = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    let canonical_named = fs::symlink_metadata(&canonical).map_err(|e| e.to_string())?;
    require(
        same_snapshot(&opened, &after)
            && same_snapshot(&after, &named)
            && same_snapshot(&named, &canonical_named),
        "native bundle input changed while reading",
    )?;
    require(
        sha256 == pin.sha256,
        "native bundle input bytes differ from expected pin",
    )?;
    Ok(proof::FileIdentity {
        path: canonical,
        sha256,
    })
}

/// Separately declared normal Buck products and source bytes, not inputs of
/// R's runtime producer. In particular, runner links that runtime; inserting
/// runner into the runtime's original input inventory would create a cycle.
/// The eight source expectations are independently authored. Generated product
/// pins record stability after their normal prerequisite actions; they do not
/// establish source/compile provenance. All eleven pins remain mandatory data,
/// and none can grant qualification. The caller separately joins runner_runtime
/// to the actual R-qualified runtime.
#[cfg(feature = "native-exec")]
#[derive(Debug)]
pub(crate) struct BundleInputs {
    expected: std::collections::BTreeMap<String, FilePin>,
    declared: std::collections::BTreeMap<String, std::path::PathBuf>,
    pub expected_file: proof::FileIdentity,
    pub declared_file: proof::FileIdentity,
}

#[cfg(feature = "native-exec")]
impl BundleInputs {
    pub fn read(expected: &Path, declared: &Path, required: &[String]) -> proof::Result<Self> {
        let (expected_file, expected) = read_json(expected)?;
        let (declared_file, declared) = read_json(declared)?;
        let value = Self {
            expected,
            declared,
            expected_file,
            declared_file,
        };
        let keys: std::collections::BTreeSet<_> = required.iter().cloned().collect();
        require(
            !keys.is_empty() && keys.len() == required.len(),
            "native bundle required keys must be nonempty and unique",
        )?;
        require(
            value
                .expected
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                == keys,
            "native bundle expected membership differs from required inputs",
        )?;
        require(
            value
                .declared
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                == keys,
            "native bundle declared membership differs from required inputs",
        )?;
        for pin in value.expected.values() {
            require(
                pin.sha256.len() == 64
                    && pin
                        .sha256
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "native bundle expected SHA256 must be 64 lowercase hex digits",
            )?;
        }
        value.verify()?;
        Ok(value)
    }

    pub fn files(&self) -> proof::Result<std::collections::BTreeMap<String, proof::FileIdentity>> {
        self.verify()?;
        let mut files = std::collections::BTreeMap::new();
        for (key, pin) in &self.expected {
            let path = &self.declared[key];
            let file =
                bundle_file(path, pin).map_err(|e| format!("native bundle input {key}: {e}"))?;
            files.insert(key.clone(), file);
        }
        self.verify()?;
        Ok(files)
    }

    /// Check an already copied file against the selected key's exact hash/size.
    /// This reads data only: it neither creates the copy nor grants authority.
    pub fn verify_copy(&self, key: &str, path: &Path) -> proof::Result<proof::FileIdentity> {
        self.verify()?;
        let pin = self
            .expected
            .get(key)
            .ok_or("native bundle copy key is absent from expected inputs")?;
        let copy =
            bundle_file(path, pin).map_err(|error| format!("native bundle copy {key}: {error}"));
        self.verify()?;
        copy
    }

    /// Recheck only the two JSON snapshots. Call files() again at source close
    /// and compare its identities to recheck the actual bundle inputs as well.
    pub fn verify(&self) -> proof::Result<()> {
        verify_snapshot(&self.expected_file)?;
        verify_snapshot(&self.declared_file)
    }
}

/// Keep build action inputs explicit; unknown, duplicate, missing flags refuse.
pub(crate) fn options(
    args: Vec<std::ffi::OsString>,
    names: &[&str],
) -> proof::Result<std::collections::BTreeMap<String, std::ffi::OsString>> {
    let mut values = std::collections::BTreeMap::new();
    require(
        args.len() == 2 * names.len(),
        "wrong option count; run --help for required native action inputs",
    )?;
    for pair in args.chunks_exact(2) {
        let flag = pair[0]
            .to_str()
            .ok_or("native option name is not UTF8; run --help")?;
        require(names.contains(&flag), "unknown native option; run --help")?;
        require(
            values.insert(flag.to_owned(), pair[1].clone()).is_none(),
            "duplicate native option; run --help",
        )?;
    }
    require(
        names.iter().all(|n| values.contains_key(*n)),
        "missing native option; run --help",
    )?;
    Ok(values)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use super::*;

    pub(super) struct TestDirectory(pub(super) PathBuf);
    impl TestDirectory {
        pub(super) fn create() -> Self {
            static SERIAL: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "hermit-native-inputs-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn identity(bytes: &[u8]) -> proof::FileIdentity {
        proof::FileIdentity {
            path: PathBuf::from("/native-input-test/record.json"),
            sha256: proof::sha256(bytes),
        }
    }

    #[test]
    fn unique_json_accepts_typed_records_and_all_json_value_kinds() {
        let bytes = br#"{"items":[1,2,3]}"#;
        let value: BTreeMap<String, Vec<u64>> = parse_snapshot(&identity(bytes), bytes).unwrap();
        assert_eq!(value["items"], [1, 2, 3]);
        let bytes = br#"{"values":[null,true,-4,5,1.5,"text",{"key":1}]}"#;
        let value: Value = parse_snapshot(&identity(bytes), bytes).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"values": [null, true, -4, 5, 1.5, "text", {"key": 1}]})
        );
        assert!(parse_snapshot::<BTreeMap<String, u64>>(&identity(bytes), bytes).is_err());
    }

    #[test]
    fn duplicate_keys_are_refused_recursively_including_null_and_escaped_keys() {
        for bytes in [
            br#"{"key":1,"key":2}"#.as_slice(),
            br#"{"opaque":{"key":null,"key":false}}"#.as_slice(),
            br#"{"opaque":[{"key":null,"key":false}]}"#.as_slice(),
            br#"{"opaque":[{"key":1,"\u006bey":2}]}"#.as_slice(),
        ] {
            let error = parse_snapshot::<Value>(&identity(bytes), bytes).unwrap_err();
            assert!(error.contains("duplicate JSON key: key"), "{error}");
        }
        let bytes = b"{} {}";
        let error = parse_snapshot::<Value>(&identity(bytes), bytes).unwrap_err();
        assert!(error.contains("trailing characters"), "{error}");
    }

    #[test]
    fn parsed_buffer_must_match_the_retained_file_identity() {
        let before = br#"{"value":1}"#;
        let changed = br#"{"value":2}"#;
        assert_eq!(before.len(), changed.len());
        let error = parse_snapshot::<Value>(&identity(before), changed).unwrap_err();
        assert!(error.contains("buffer digest differs"), "{error}");
    }

    #[test]
    fn exact_byte_limit_passes_and_one_extra_byte_is_never_truncated() {
        let root = TestDirectory::create();
        let path = root.0.join("bounded.json");
        let mut bytes = vec![b' '; MAX_INPUT_BYTES];
        bytes[..2].copy_from_slice(b"{}");
        fs::write(&path, &bytes).unwrap();
        let (file, value) = read_json::<Value>(&path).unwrap();
        assert_eq!(value, serde_json::json!({}));
        assert_eq!(file.sha256, proof::sha256(&bytes));
        bytes.push(b' ');
        let error = parse_snapshot::<Value>(&identity(&bytes), &bytes).unwrap_err();
        assert!(error.contains("16 MiB byte bound"), "{error}");
        fs::write(&path, &bytes).unwrap();
        let error = read_json::<Value>(&path).unwrap_err();
        assert!(error.contains("16 MiB byte bound"), "{error}");
    }

    #[test]
    fn retained_input_reverification_refuses_changed_bytes() {
        let root = TestDirectory::create();
        let path = root.0.join("record.json");
        fs::write(&path, br#"{"value":1}"#).unwrap();
        let (file, value) = read_json::<Value>(&path).unwrap();
        assert_eq!(value, serde_json::json!({"value": 1}));
        verify_snapshot(&file).unwrap();
        fs::write(&path, br#"{"value":2}"#).unwrap();
        let error = verify_snapshot(&file).unwrap_err();
        assert!(error.contains("identity changed"), "{error}");
    }

    #[test]
    fn source_records_refuse_symlinks_directories_and_fifos() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let root = TestDirectory::create();
        let target = root.0.join("record.json");
        fs::write(&target, b"{}").unwrap();
        let link = root.0.join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_json::<Value>(&link).is_err());
        assert!(read_json::<Value>(&root.0).is_err());
        let fifo = root.0.join("record.fifo");
        let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_name is a live NUL-terminated pathname for the private
        // test directory. mkfifo retains no pointer after this call.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        assert!(read_json::<Value>(&fifo).is_err());
    }

    #[test]
    fn native_options_accept_exact_inputs_and_refuse_duplicate_unknown_or_missing_flags() {
        let names = ["--expected", "--declared"];
        let args = |values: &[&str]| values.iter().map(OsString::from).collect();
        let selected = options(
            args(&["--expected", "e.json", "--declared", "d.json"]),
            &names,
        )
        .unwrap();
        assert_eq!(selected["--expected"], OsString::from("e.json"));
        assert_eq!(selected["--declared"], OsString::from("d.json"));
        for (values, message) in [
            (
                vec!["--expected", "e.json", "--expected", "other.json"],
                "duplicate native option",
            ),
            (
                vec!["--expected", "e.json", "--foreign", "d.json"],
                "unknown native option",
            ),
            (vec!["--expected", "e.json"], "wrong option count"),
            (
                vec!["--expected", "e.json", "--declared"],
                "wrong option count",
            ),
        ] {
            let error = options(args(&values), &names).unwrap_err();
            assert!(error.contains(message), "{error}");
            assert!(error.contains("--help"), "{error}");
        }
    }
}

#[cfg(all(test, feature = "native-exec"))]
mod bundle_tests {
    use std::path::PathBuf;

    use super::tests::TestDirectory;
    use super::*;

    struct Fixture {
        _root: TestDirectory,
        expected_path: PathBuf,
        declared_path: PathBuf,
        expected: Value,
        declared: Value,
        required: Vec<String>,
    }
    impl Fixture {
        fn create() -> Self {
            let root = TestDirectory::create();
            let mut required = vec![
                "bootstrap".to_owned(),
                "runner".to_owned(),
                "runner_runtime".to_owned(),
            ];
            required.extend(
                [
                    "private-bootstrap.c",
                    "private-control.h",
                    "private-entry.S",
                    "private-fs.S",
                    "private-kernel-state.c",
                    "private-maps.c",
                    "private-bootstrap.ld",
                    "private-runner.c",
                ]
                .map(|name| format!("rv/bootstrap/{name}")),
            );
            let mut expected = Map::new();
            let mut declared = Map::new();
            for (index, key) in required.iter().enumerate() {
                let path = root.0.join(format!("input-{index}"));
                let bytes = format!("actual declared bundle input {key}\n").into_bytes();
                fs::write(&path, &bytes).unwrap();
                expected.insert(
                    key.clone(),
                    serde_json::json!({
                        "sha256": proof::sha256(&bytes), "size": bytes.len(),
                    }),
                );
                declared.insert(key.clone(), serde_json::to_value(path).unwrap());
            }
            let value = Self {
                expected_path: root.0.join("bundle-expected.json"),
                declared_path: root.0.join("bundle-declared.json"),
                _root: root,
                expected: Value::Object(expected),
                declared: Value::Object(declared),
                required,
            };
            value.write();
            value
        }
        fn write(&self) {
            fs::write(
                &self.expected_path,
                serde_json::to_vec(&self.expected).unwrap(),
            )
            .unwrap();
            fs::write(
                &self.declared_path,
                serde_json::to_vec(&self.declared).unwrap(),
            )
            .unwrap();
        }
        fn read(&self) -> proof::Result<BundleInputs> {
            BundleInputs::read(&self.expected_path, &self.declared_path, &self.required)
        }
        fn path(&self, key: &str) -> PathBuf {
            PathBuf::from(self.declared[key].as_str().unwrap())
        }
    }

    #[test]
    fn exact_bundle_maps_accept_and_leave_all_input_bytes_untouched() {
        let mut fixture = Fixture::create();
        fixture.required.reverse();
        let original: Vec<_> = fixture
            .required
            .iter()
            .map(|key| {
                let path = fixture.path(key);
                (
                    key.clone(),
                    fs::read(&path).unwrap(),
                    fs::symlink_metadata(path).unwrap(),
                )
            })
            .collect();
        let inputs = fixture.read().unwrap();
        let files = inputs.files().unwrap();
        assert_eq!(files.len(), 11);
        inputs.verify().unwrap();
        assert_eq!(inputs.files().unwrap(), files);
        for (key, bytes, metadata) in original {
            let path = fixture.path(&key);
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(files[&key].sha256, proof::sha256(&bytes));
            assert_eq!(files[&key].path, fs::canonicalize(&path).unwrap());
            assert!(same_snapshot(
                &metadata,
                &fs::symlink_metadata(path).unwrap()
            ));
        }
    }

    #[test]
    fn bundle_maps_require_each_exact_member_on_both_sides() {
        for side in ["expected", "declared", "both"] {
            for extra in [false, true] {
                let mut fixture = Fixture::create();
                for (name, value) in [
                    ("expected", &mut fixture.expected),
                    ("declared", &mut fixture.declared),
                ] {
                    if side == name || side == "both" {
                        let map = value.as_object_mut().unwrap();
                        if extra {
                            map.insert("unlisted".into(), map["bootstrap"].clone());
                        } else {
                            map.remove("bootstrap");
                        }
                    }
                }
                fixture.write();
                let error = fixture.read().unwrap_err();
                assert!(
                    error.contains("membership differs"),
                    "{side}/{extra}: {error}"
                );
            }
        }
        let fixture = Fixture::create();
        let mut duplicate = fixture.required.clone();
        duplicate.push(duplicate[0].clone());
        for required in [vec![], duplicate] {
            let error =
                BundleInputs::read(&fixture.expected_path, &fixture.declared_path, &required)
                    .unwrap_err();
            assert!(error.contains("nonempty and unique"), "{error}");
        }
    }

    #[test]
    fn bundle_pins_refuse_unknown_missing_and_malformed_fields() {
        let fixture = Fixture::create();
        let good = fixture.expected["bootstrap"].clone();
        let mut cases = Vec::new();
        for key in ["sha256", "size"] {
            let mut pin = good.clone();
            pin.as_object_mut().unwrap().remove(key);
            cases.push(pin);
        }
        let mut unknown = good.clone();
        unknown["authority"] = serde_json::json!("receipt");
        cases.push(unknown);
        for hash in ["", "a", &"A".repeat(64), &"g".repeat(64), &"a".repeat(63)] {
            let mut pin = good.clone();
            pin["sha256"] = serde_json::json!(hash);
            cases.push(pin);
        }
        for size in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!(true),
            serde_json::json!("10"),
        ] {
            let mut pin = good.clone();
            pin["size"] = size;
            cases.push(pin);
        }
        for pin in cases {
            let mut expected = fixture.expected.clone();
            expected["bootstrap"] = pin.clone();
            fs::write(
                &fixture.expected_path,
                serde_json::to_vec(&expected).unwrap(),
            )
            .unwrap();
            assert!(fixture.read().is_err(), "accepted malformed pin {pin}");
        }
        fixture.write();
        let mut declared = fixture.declared.clone();
        declared["bootstrap"] = serde_json::json!({"path": fixture.path("bootstrap")});
        fs::write(
            &fixture.declared_path,
            serde_json::to_vec(&declared).unwrap(),
        )
        .unwrap();
        assert!(fixture.read().is_err());
    }

    #[test]
    fn bundle_maps_reject_duplicate_members_and_nested_pin_keys() {
        let fixture = Fixture::create();
        let original = fs::read_to_string(&fixture.expected_path).unwrap();
        let duplicate_member = format!(
            "{{\"bootstrap\":{},{}",
            fixture.expected["bootstrap"],
            &original[1..]
        );
        let digest = fixture.expected["bootstrap"]["sha256"].as_str().unwrap();
        let field = format!("\"sha256\":\"{digest}\"");
        let duplicate_pin = original.replacen(&field, &format!("{field},{field}"), 1);
        for bytes in [duplicate_member, duplicate_pin] {
            fs::write(&fixture.expected_path, bytes).unwrap();
            let error = fixture.read().unwrap_err();
            assert!(error.contains("duplicate JSON key"), "{error}");
        }
    }

    #[test]
    fn bundle_files_refuse_wrong_digest_size_and_later_actual_byte_changes() {
        for field in ["sha256", "size"] {
            let mut fixture = Fixture::create();
            fixture.expected["bootstrap"][field] = if field == "sha256" {
                serde_json::json!("0".repeat(64))
            } else {
                serde_json::json!(fixture.expected["bootstrap"]["size"].as_u64().unwrap() + 1)
            };
            fixture.write();
            let inputs = fixture.read().unwrap();
            assert!(inputs.files().is_err(), "accepted wrong {field}");
        }
        let fixture = Fixture::create();
        let inputs = fixture.read().unwrap();
        inputs.files().unwrap();
        let path = fixture.path("runner");
        let mut changed = fs::read(&path).unwrap();
        changed[0] ^= 1;
        fs::write(&path, changed).unwrap();
        inputs.verify().unwrap(); // JSON checks do not replace the files() close check.
        let error = inputs.files().unwrap_err();
        assert!(error.contains("bytes differ"), "{error}");
    }

    #[test]
    fn bundle_copy_requires_selected_bytes_size_and_expected_key() {
        let fixture = Fixture::create();
        let inputs = fixture.read().unwrap();
        for key in ["bootstrap", "runner"] {
            let source = fixture.path(key);
            let staged = source.with_extension("staged-copy");
            let bytes = fs::read(&source).unwrap();
            fs::copy(&source, &staged).unwrap();
            let copy = inputs.verify_copy(key, &staged).unwrap();
            assert_eq!(copy.path, fs::canonicalize(&staged).unwrap());
            assert_ne!(copy.path, fs::canonicalize(&source).unwrap());
            assert_eq!(copy.sha256, proof::sha256(&bytes));
            assert_eq!(fs::metadata(&staged).unwrap().len(), bytes.len() as u64);

            let mut changed = bytes.clone();
            changed[0] ^= 1;
            fs::write(&staged, &changed).unwrap();
            let error = inputs.verify_copy(key, &staged).unwrap_err();
            assert!(
                error.contains("bytes differ") && error.contains("expected"),
                "{error}"
            );
            fs::write(&staged, &bytes[..bytes.len() - 1]).unwrap();
            let error = inputs.verify_copy(key, &staged).unwrap_err();
            assert!(
                error.contains("size differs") && error.contains("expected"),
                "{error}"
            );
            fs::write(&staged, &bytes).unwrap();
            let error = inputs.verify_copy("unlisted", &staged).unwrap_err();
            assert!(
                error.contains("key") && error.contains("expected"),
                "{error}"
            );
            inputs.verify_copy(key, &staged).unwrap();
            assert_eq!(fs::read(source).unwrap(), bytes);
        }
    }

    #[test]
    fn bundle_copy_refuses_changed_json_snapshots() {
        let fixture = Fixture::create();
        let source = fixture.path("bootstrap");
        let staged = source.with_extension("staged-copy");
        fs::copy(&source, &staged).unwrap();
        for path in [&fixture.expected_path, &fixture.declared_path] {
            fixture.write();
            let inputs = fixture.read().unwrap();
            inputs.verify_copy("bootstrap", &staged).unwrap();
            let mut changed = fs::read(path).unwrap();
            changed.push(b' ');
            fs::write(path, changed).unwrap();
            assert!(inputs.verify_copy("bootstrap", &staged).is_err());
        }
    }

    #[test]
    fn bundle_digest_checks_exact_length_and_stops_after_one_extra_byte() {
        use std::io::Cursor;

        for bytes in [vec![], vec![5], vec![7; 100]] {
            let mut reader = Cursor::new(&bytes);
            assert_eq!(
                bundle_digest(&mut reader, bytes.len() as u64).unwrap(),
                proof::sha256(&bytes)
            );
            assert_eq!(reader.position(), bytes.len() as u64);
        }
        let mut oversized = Cursor::new(vec![9; 4096]);
        assert!(bundle_digest(&mut oversized, 31).is_err());
        assert_eq!(oversized.position(), 32);
        let mut undersized = Cursor::new(vec![9; 30]);
        assert!(bundle_digest(&mut undersized, 31).is_err());
        assert_eq!(undersized.position(), 30);
    }

    #[test]
    fn actual_bundle_products_are_not_limited_to_the_json_record_size() {
        let mut fixture = Fixture::create();
        let bytes = vec![7; MAX_INPUT_BYTES + 1];
        fs::write(fixture.path("bootstrap"), &bytes).unwrap();
        fixture.expected["bootstrap"] = serde_json::json!({
            "sha256": proof::sha256(&bytes), "size": bytes.len(),
        });
        fixture.write();
        let files = fixture.read().unwrap().files().unwrap();
        assert_eq!(files["bootstrap"].sha256, proof::sha256(&bytes));
    }

    #[test]
    fn bundle_actual_inputs_refuse_symlinks_directories_fifos_and_missing_files() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        for kind in ["symlink", "directory", "fifo", "missing"] {
            let fixture = Fixture::create();
            let inputs = fixture.read().unwrap();
            let path = fixture.path("bootstrap");
            let matching = path.with_extension("same-bytes");
            fs::copy(&path, &matching).unwrap();
            fs::remove_file(&path).unwrap();
            match kind {
                "symlink" => std::os::unix::fs::symlink(&matching, &path).unwrap(),
                "directory" => fs::create_dir(&path).unwrap(),
                "fifo" => {
                    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
                    // SAFETY: the private test pathname is live and NUL-terminated;
                    // mkfifo retains no pointer after this call.
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
                "missing" => (),
                _ => unreachable!(),
            }
            assert!(inputs.files().is_err(), "accepted {kind}");
        }
    }

    #[test]
    fn bundle_snapshot_checks_refuse_changes_to_either_json_record() {
        let fixture = Fixture::create();
        for path in [&fixture.expected_path, &fixture.declared_path] {
            fixture.write();
            let inputs = fixture.read().unwrap();
            let mut bytes = fs::read(path).unwrap();
            bytes.push(b' ');
            fs::write(path, bytes).unwrap();
            assert!(inputs.verify().is_err());
            assert!(inputs.files().is_err());
        }
    }

    #[test]
    fn source_literal_check_requires_retained_identity_and_exact_bytes() {
        let fixture = Fixture::create();
        let key = "rv/bootstrap/private-bootstrap.c";
        let files = fixture.read().unwrap().files().unwrap();
        let identity = &files[key];
        let original = fs::read(&identity.path).unwrap();
        verify_source_file_bytes(identity, &original).unwrap();
        let mut different = original.clone();
        different[0] ^= 1;
        let error = verify_source_file_bytes(identity, &different).unwrap_err();
        assert!(
            error.contains("source bytes differ from compiled digest inputs"),
            "{error}"
        );
        fs::write(&identity.path, &different).unwrap();
        let error = verify_source_file_bytes(identity, &different).unwrap_err();
        assert!(error.contains("identity changed"), "{error}");
        fs::write(&identity.path, &original).unwrap();
        let matching = identity.path.with_extension("same-bytes");
        fs::rename(&identity.path, &matching).unwrap();
        std::os::unix::fs::symlink(&matching, &identity.path).unwrap();
        assert!(verify_source_file_bytes(identity, &original).is_err());
    }
}
