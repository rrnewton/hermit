#!/usr/bin/env -S rust-script --force
//! Bounded, opt-in diagnostics for run-nextest-counted.sh; never result authority.
//!
//! The destination must be a new absolute directory with an existing parent.
//! Neither source nor destination traversal follows caller-controlled symlinks.
//! capture.json records incomplete copies; consumers must require capture_complete.
//!
//! ```cargo
//! [dependencies]
//! serde_json = "1"
//! ```

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::fs::DirBuilder;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::Value;
use serde_json::json;

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_FILES: usize = 32;
const MANIFEST_RESERVE: u64 = 64 * 1024;
// Linux flags: this producer, like Nextest's CPU wrapper, runs on Linux only.
const O_NONBLOCK: i32 = 0o4000;
const O_DIRECTORY: i32 = 0o200000;
const O_NOFOLLOW: i32 = 0o400000;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

// Anchoring each lookup to an open descriptor also prevents an ancestor rename
// or symlink swap from redirecting subsequent reads/writes. /proc/self/fd is the
// kernel-owned descriptor namespace, not a caller-supplied symlink to follow.
fn beneath(directory: &File, name: &OsStr) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name)
}

fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW | O_NONBLOCK)
        .open(path)
}

fn open_absolute_directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() || path.as_os_str().len() > 4096 {
        return Err(invalid(
            "directory must be an absolute path of at most 4096 bytes",
        ));
    }
    let mut directory = open_directory(Path::new("/"))?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => directory = open_directory(&beneath(&directory, name))?,
            _ => return Err(invalid("directory contains a non-normal path component")),
        }
    }
    Ok(directory)
}

fn new_directory(parent: &File, name: &OsStr) -> io::Result<File> {
    let path = beneath(parent, name);
    DirBuilder::new().mode(0o700).create(&path)?;
    open_directory(&path)
}

fn create_destination(path: &Path) -> io::Result<File> {
    let name = path
        .file_name()
        .ok_or_else(|| invalid("destination needs a directory name"))?;
    let parent = open_absolute_directory(
        path.parent()
            .ok_or_else(|| invalid("destination needs an existing parent"))?,
    )?;
    new_directory(&parent, name)
}

fn new_file(directory: &File, name: &OsStr) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(beneath(directory, name))
}

fn error_text(error: &io::Error) -> String {
    // Keep the manifest bounded even when the OS embeds an unexpectedly long path.
    error.to_string().chars().take(512).collect()
}

struct Capture {
    files: Vec<Value>,
    bytes: u64,
    complete: bool,
}

impl Capture {
    fn copy(&mut self, source: &File, output: &File, name: &OsStr, relative: String) {
        let mut record = json!({"path": relative, "state": "capture_failed", "captured_bytes": 0});
        let result = self.copy_file(source, output, name, &mut record);
        if let Err(error) = result {
            record["state"] = json!(if error.kind() == io::ErrorKind::NotFound {
                "absent"
            } else {
                "capture_failed"
            });
            record["error"] = json!(error_text(&error));
        }
        if record["state"] != "complete" {
            self.complete = false;
        }
        self.files.push(record);
    }

    fn copy_file(
        &mut self,
        source: &File,
        output: &File,
        name: &OsStr,
        record: &mut Value,
    ) -> io::Result<()> {
        let path = beneath(source, name);
        let before = fs::symlink_metadata(&path)?;
        if !before.is_file() || before.nlink() != 1 {
            return Err(invalid(
                "source is not a regular file with exactly one link",
            ));
        }
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(path)?;
        let metadata = input.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || before.dev() != metadata.dev()
            || before.ino() != metadata.ino()
        {
            return Err(invalid("source changed while opening"));
        }
        record["source_bytes"] = json!(metadata.len());
        let limit = MAX_FILE_BYTES.min(MAX_TOTAL_BYTES - MANIFEST_RESERVE - self.bytes);
        if limit == 0 {
            record["state"] = json!("over_budget");
            return Ok(());
        }
        let mut destination = new_file(output, name)?;
        let mut captured = 0;
        let copied = (|| -> io::Result<()> {
            let mut buffer = [0u8; 16384];
            while captured < limit {
                let length = buffer.len().min((limit - captured) as usize);
                let count = input.read(&mut buffer[..length])?;
                if count == 0 {
                    break;
                }
                // Count only bytes actually written, including a later partial error.
                let mut written = 0;
                while written < count {
                    let written_now = destination.write(&buffer[written..count])?;
                    if written_now == 0 {
                        return Err(io::ErrorKind::WriteZero.into());
                    }
                    written += written_now;
                    captured += written_now as u64;
                }
            }
            destination.sync_all()?;
            let extra = input.read(&mut [0u8; 1])?;
            let after = input.metadata()?;
            record["state"] = json!(if extra != 0 {
                "over_budget"
            } else if metadata.len() != captured
                || metadata.len() != after.len()
                || metadata.mtime() != after.mtime()
                || metadata.mtime_nsec() != after.mtime_nsec()
            {
                "partial"
            } else {
                "complete"
            });
            Ok(())
        })();
        self.bytes += captured;
        record["captured_bytes"] = json!(captured);
        if let Err(error) = copied {
            record["state"] = json!("partial");
            record["error"] = json!(error_text(&error));
        }
        Ok(())
    }

    fn attempts(&mut self, source: &File, output: &File) -> io::Result<Value> {
        let source = open_directory(&beneath(source, OsStr::new("attempts")))?;
        let output = new_directory(output, OsStr::new("attempts"))?;
        let capacity = MAX_FILES - 1 - self.files.len(); // reserve capture.json
        let mut entries = fs::read_dir(beneath(&source, OsStr::new(".")))?;
        // Read one extra entry to detect excess without enumerating an unbounded
        // directory. Sorting this bounded subset makes its presentation stable.
        let mut names = Vec::new();
        for entry in entries.by_ref().take(capacity + 1) {
            names.push(entry?.file_name());
        }
        let over_budget = names.len() > capacity;
        names.sort();
        names.truncate(capacity);
        for name in names {
            let Some(text) = name.to_str().filter(|name| name.ends_with(".json")) else {
                self.complete = false;
                self.files.push(json!({"path": format!("attempts/{}", name.to_string_lossy()), "state": "capture_failed", "captured_bytes": 0, "error": "unexpected attempt filename"}));
                continue;
            };
            self.copy(&source, &output, &name, format!("attempts/{text}"));
        }
        if over_budget {
            self.complete = false;
        }
        Ok(
            json!({"state": if over_budget { "over_budget" } else { "complete" }, "entry_limit": capacity}),
        )
    }
}

fn status(argument: &str) -> Result<Option<u8>, String> {
    if argument == "-" {
        return Ok(None);
    }
    argument
        .parse()
        .map(Some)
        .map_err(|_| format!("invalid observed status {argument:?}"))
}

fn run() -> Result<bool, String> {
    let arguments = env::args_os()
        .skip(1)
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| "arguments must be UTF-8")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if arguments.len() != 4 {
        return Err("usage: nextest-retain-diagnostics.rs OWNED_TEMP_DIRECTORY NEXTEST_STATUS_OR_DASH WRITER_STATUS_OR_DASH WRAPPER_STATUS".into());
    }
    let nextest_status = status(&arguments[1])?;
    let writer_status = status(&arguments[2])?;
    let wrapper_status = status(&arguments[3])?.ok_or("wrapper status must be observed")?;
    let destination =
        env::var("HERMIT_NEXTEST_RETAIN_DIAGNOSTICS_DIR").map_err(|error| error.to_string())?;
    let source_path = Path::new(&arguments[0]);
    if source_path.is_absolute() && Path::new(&destination).starts_with(source_path) {
        return Err(
            "diagnostics destination is inside the temporary directory that cleanup removes".into(),
        );
    }
    let output = create_destination(Path::new(&destination))
        .map_err(|error| format!("cannot exclusively create diagnostics directory: {error}"))?;
    let mut capture = Capture {
        files: Vec::new(),
        bytes: 0,
        complete: true,
    };
    let mut source_error = None;
    let attempts = match open_absolute_directory(source_path) {
        Ok(source) => {
            for name in [
                "events.jsonl",
                "inventory.json",
                "binary-map.json",
                "nextest.toml",
            ] {
                capture.copy(&source, &output, OsStr::new(name), name.to_string());
            }
            match capture.attempts(&source, &output) {
                Ok(state) => state,
                Err(error) => {
                    capture.complete = false;
                    json!({"state": if error.kind() == io::ErrorKind::NotFound { "absent" } else { "capture_failed" }, "error": error_text(&error)})
                }
            }
        }
        Err(error) => {
            capture.complete = false;
            source_error = Some(error_text(&error));
            json!({"state": "absent", "error": "source directory unavailable"})
        }
    };
    let manifest = json!({
        "schema": 1, "diagnostic_only": true,
        "nextest_status": nextest_status, "writer_status": writer_status,
        "wrapper_status": wrapper_status, "capture_complete": capture.complete,
        "limits": {"max_total_bytes": MAX_TOTAL_BYTES, "max_file_bytes": MAX_FILE_BYTES, "max_files": MAX_FILES},
        "captured_data_bytes": capture.bytes, "source_error": source_error,
        "files": capture.files, "attempts": attempts,
    });
    let mut bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MANIFEST_RESERVE {
        return Err("capture manifest exceeded reserved bound".into());
    }
    let mut file =
        new_file(&output, OsStr::new("capture.json")).map_err(|error| error.to_string())?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| output.sync_all())
        .map_err(|error| format!("cannot publish capture manifest: {error}"))?;
    Ok(capture.complete)
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("nextest-retain-diagnostics: incomplete capture; inspect capture.json");
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("nextest-retain-diagnostics: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    struct Fixture {
        path: PathBuf,
        directory: File,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = env::temp_dir().join(format!(
                "hermit-nextest-retain-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            // Never adopt or remove an existing fixture, even after a prior crash.
            DirBuilder::new().mode(0o700).create(&path).unwrap();
            let path = path.canonicalize().unwrap();
            let directory = open_absolute_directory(&path).unwrap();
            Self { path, directory }
        }

        fn directory(&self, name: &str) -> File {
            new_directory(&self.directory, OsStr::new(name)).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).unwrap();
        }
    }

    fn capture() -> Capture {
        Capture {
            files: Vec::new(),
            bytes: 0,
            complete: true,
        }
    }

    fn sparse(directory: &File, name: &str, length: u64) {
        new_file(directory, OsStr::new(name))
            .unwrap()
            .set_len(length)
            .unwrap();
    }

    #[test]
    fn complete_copy_preserves_bytes_and_missing_input_is_explicit() {
        let fixture = Fixture::new();
        let source = fixture.directory("source");
        let output = fixture.directory("output");
        let bytes = b"{\"type\":\"test\",\"event\":\"failed\"}\n";
        new_file(&source, OsStr::new("events.jsonl"))
            .unwrap()
            .write_all(bytes)
            .unwrap();
        let mut capture = capture();
        capture.copy(
            &source,
            &output,
            OsStr::new("events.jsonl"),
            "events.jsonl".into(),
        );
        assert!(capture.complete);
        assert_eq!(capture.files[0]["state"], "complete");
        assert_eq!(capture.bytes, bytes.len() as u64);
        assert_eq!(
            fs::read(fixture.path.join("output/events.jsonl")).unwrap(),
            bytes
        );
        capture.copy(
            &source,
            &output,
            OsStr::new("inventory.json"),
            "inventory.json".into(),
        );
        assert!(!capture.complete);
        assert_eq!(capture.files[1]["state"], "absent");
        assert_eq!(capture.files[1]["captured_bytes"], 0);
        assert!(!fixture.path.join("output/inventory.json").exists());
    }

    #[test]
    fn existing_destination_is_unchanged() {
        let fixture = Fixture::new();
        let destination = fixture.path.join("retained");
        let output = create_destination(&destination).unwrap();
        new_file(&output, OsStr::new("sentinel"))
            .unwrap()
            .write_all(b"keep exactly these bytes")
            .unwrap();
        assert_eq!(
            create_destination(&destination).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            fs::read(destination.join("sentinel")).unwrap(),
            b"keep exactly these bytes"
        );
        assert_eq!(fs::read_dir(destination).unwrap().count(), 1);
    }

    #[test]
    fn symlink_ancestors_and_inputs_are_refused() {
        let fixture = Fixture::new();
        fixture.directory("elsewhere");
        symlink("elsewhere", fixture.path.join("alias")).unwrap();
        assert!(create_destination(&fixture.path.join("alias/retained")).is_err());
        assert!(!fixture.path.join("elsewhere/retained").exists());
        assert!(open_absolute_directory(&fixture.path.join("alias")).is_err());

        let source = fixture.directory("source");
        let output = fixture.directory("output");
        fs::write(fixture.path.join("secret"), b"must not be copied").unwrap();
        symlink("../secret", fixture.path.join("source/events.jsonl")).unwrap();
        let mut capture = capture();
        capture.copy(
            &source,
            &output,
            OsStr::new("events.jsonl"),
            "events.jsonl".into(),
        );
        assert!(!capture.complete);
        assert_eq!(capture.bytes, 0);
        assert_eq!(capture.files[0]["state"], "capture_failed");
        assert!(!fixture.path.join("output/events.jsonl").exists());

        symlink("../elsewhere", fixture.path.join("source/attempts")).unwrap();
        assert!(capture.attempts(&source, &output).is_err());
        assert!(!fixture.path.join("output/attempts").exists());
    }

    #[test]
    fn per_file_bound_retains_prefix_and_reports_truncation() {
        let fixture = Fixture::new();
        let source = fixture.directory("source");
        let output = fixture.directory("output");
        sparse(&source, "events.jsonl", MAX_FILE_BYTES + 1);
        let mut capture = capture();
        capture.copy(
            &source,
            &output,
            OsStr::new("events.jsonl"),
            "events.jsonl".into(),
        );
        assert!(!capture.complete);
        assert_eq!(capture.files[0]["state"], "over_budget");
        assert_eq!(capture.files[0]["source_bytes"], MAX_FILE_BYTES + 1);
        assert_eq!(capture.files[0]["captured_bytes"], MAX_FILE_BYTES);
        assert_eq!(capture.bytes, MAX_FILE_BYTES);
        assert_eq!(
            fs::metadata(fixture.path.join("output/events.jsonl"))
                .unwrap()
                .len(),
            MAX_FILE_BYTES
        );
    }

    #[test]
    fn total_bound_includes_space_for_manifest_and_stops_copying() {
        let fixture = Fixture::new();
        let source = fixture.directory("source");
        let output = fixture.directory("output");
        let mut capture = capture();
        for index in 0..5 {
            let name = format!("{index}.json");
            sparse(&source, &name, MAX_FILE_BYTES);
            capture.copy(&source, &output, OsStr::new(&name), name.clone());
        }
        assert!(!capture.complete);
        assert_eq!(capture.bytes, MAX_TOTAL_BYTES - MANIFEST_RESERVE);
        for file in &capture.files[..3] {
            assert_eq!(file["state"], "complete");
            assert_eq!(file["captured_bytes"], MAX_FILE_BYTES);
        }
        assert_eq!(capture.files[3]["state"], "over_budget");
        assert_eq!(
            capture.files[3]["captured_bytes"],
            MAX_FILE_BYTES - MANIFEST_RESERVE
        );
        assert_eq!(capture.files[4]["state"], "over_budget");
        assert_eq!(capture.files[4]["captured_bytes"], 0);
        assert!(!fixture.path.join("output/4.json").exists());
        let actual_bytes: u64 = fs::read_dir(fixture.path.join("output"))
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert_eq!(actual_bytes, capture.bytes);
    }

    #[test]
    fn attempt_count_bound_reserves_manifest_and_fixed_inputs() {
        let fixture = Fixture::new();
        let source = fixture.directory("source");
        let output = fixture.directory("output");
        let attempts = new_directory(&source, OsStr::new("attempts")).unwrap();
        let mut capture = capture();
        for name in [
            "events.jsonl",
            "inventory.json",
            "binary-map.json",
            "nextest.toml",
        ] {
            sparse(&source, name, 1);
            capture.copy(&source, &output, OsStr::new(name), name.to_string());
        }
        for index in 0..MAX_FILES {
            sparse(&attempts, &format!("{index:02}.json"), 1);
        }
        let listing = capture.attempts(&source, &output).unwrap();
        assert!(!capture.complete);
        assert_eq!(listing["state"], "over_budget");
        assert_eq!(listing["entry_limit"], MAX_FILES - 5);
        assert_eq!(capture.files.len(), MAX_FILES - 1);
        assert_eq!(capture.bytes, (MAX_FILES - 1) as u64);
        assert!(capture.files.iter().all(|file| file["state"] == "complete"));
        assert_eq!(
            fs::read_dir(fixture.path.join("output/attempts"))
                .unwrap()
                .count(),
            MAX_FILES - 5
        );
    }
}
