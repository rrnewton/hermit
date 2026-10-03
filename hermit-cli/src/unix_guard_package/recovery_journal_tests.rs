//! Admission consumes bytes emitted by the actual C append/terminal functions.
//! Their existing host fixture controls BPF/process premises; this is not a
//! native policy load or a substitute for the official guard terminal proof.
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::OnceLock;

use super::*;

const INCARNATION: u64 = 7;
const JOURNAL: &str = "ugb1-0000000000000007";
const LABEL: &str = "guard-b1-00000000000000070000000000000009";

fn native_journal() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("producer.c");
        let executable = temp.path().join("producer");
        let output = temp.path().join(JOURNAL);
        let native_sources = Path::new(env!("CARGO_MANIFEST_DIR")).join("network-provider/unix");
        let phase_fixture =
            std::fs::read_to_string(native_sources.join("keeper-phase-tests.c")).unwrap();
        assert_eq!(phase_fixture.matches("int main(void)").count(), 1);
        // Only rename the existing fixture's entry point; its production
        // functions, wrapped premises and assertions are all retained.
        std::fs::write(
            temp.path().join("phase-fixture.c"),
            phase_fixture.replacen("int main(void)", "int retained_phase_controls(void)", 1),
        )
        .unwrap();
        std::fs::write(
            &source,
            r#"
#include "phase-fixture.c"
int main(int argc, char **argv) {
    assert(argc==2 && sizeof(struct recovery_record)==104);
    phase_fresh();
    assert(append(&subject,RECORD_BEGIN,0,0,0,NULL,0)==0);
    struct ug_terminal_receipt proof=phase_prepare();
    phase_close(proof,true);
    FILE *out=fopen(argv[1],"wb");assert(out);
    assert(fwrite(journal,sizeof(journal[0]),writes,out)==writes);
    assert(fclose(out)==0);
    return 0;
}
"#,
        )
        .unwrap();
        let mut compile = Command::new("cc");
        compile
            .args([
                "-std=gnu11",
                "-O2",
                "-ffunction-sections",
                "-fdata-sections",
                "-Wl,--gc-sections",
            ])
            .arg("-I")
            .arg(&native_sources)
            .arg(&source)
            .arg("-o")
            .arg(&executable);
        for symbol in [
            "syscall",
            "poll",
            "write",
            "fdatasync",
            "close",
            "unlinkat",
            "fstat",
            "fstatat",
            "fcntl",
            "clock_gettime",
        ] {
            compile.arg(format!("-Wl,--wrap={symbol}"));
        }
        let compiled = compile.output().unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let written = Command::new(executable).arg(&output).output().unwrap();
        assert!(
            written.status.success(),
            "{}",
            String::from_utf8_lossy(&written.stderr)
        );
        std::fs::read(output).unwrap()
    })
}

struct Fixture {
    _temp: tempfile::TempDir,
    pins: PathBuf,
    receipts: PathBuf,
    pin_fd: File,
    receipt_fd: File,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let pins = temp.path().join("pins");
        let receipts = temp.path().join("receipts");
        for directory in [&pins, &receipts] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(directory)
                .unwrap();
        }
        Self {
            pin_fd: File::open(&pins).unwrap(),
            receipt_fd: File::open(&receipts).unwrap(),
            _temp: temp,
            pins,
            receipts,
        }
    }
    fn write(&self, name: &str, bytes: &[u8]) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.receipts.join(name))
            .unwrap();
        file.write_all(bytes).unwrap();
    }
    fn census(&self) -> io::Result<GuardAdmissionCensus> {
        guard_admission_census(self.pin_fd.as_fd(), self.receipt_fd.as_fd(), unsafe {
            libc::getuid()
        })
    }
    fn terminal(&self, label: &str, change: impl FnOnce(&mut serde_json::Value)) {
        let rows = native_journal().len() / 104;
        let mut row = serde_json::json!({"schema":1,"stage":"terminal","guard":{"incarnation":INCARNATION,"terminal":{"record_ordinal":rows-2,"proof_sequence":10},"readback":{"closed_ns":1_000_000_000u64,"deadline_ns":2_000_000_000u64}}});
        change(&mut row);
        self.write(
            &format!("{label}.terminal.jsonl"),
            format!("{row}\n").as_bytes(),
        );
        self.write(&format!("{label}.stdout.log"), b"");
        self.write(&format!("{label}.stderr.log"), b"");
    }
}

#[test]
fn actual_keeper_terminal_bytes_join_unique_cli_receipt_without_deleting_evidence() {
    let fixture = Fixture::new();
    fixture.write(JOURNAL, native_journal());
    assert_eq!(
        fixture.census().unwrap().unresolved,
        BTreeSet::from(["0000000000000007".into()])
    );
    fixture.terminal(LABEL, |_| {});
    let completed = fixture.census().unwrap();
    assert!(completed.unresolved.is_empty());
    assert_eq!(completed.journals.len(), 1);
    assert_eq!(completed.receipts.len(), 4);
    assert_eq!(
        std::fs::read(fixture.receipts.join(JOURNAL)).unwrap(),
        native_journal()
    );
    assert_eq!(fixture.census().unwrap(), completed);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(fixture.pins.join(JOURNAL))
        .unwrap();
    assert_eq!(
        fixture.census().unwrap().unresolved.len(),
        1,
        "live pin stays unresolved despite both terminal artifacts"
    );
}

#[test]
fn incomplete_keeper_prefixes_and_mismatched_terminal_fields_never_complete() {
    let fixture = Fixture::new();
    fixture.terminal(LABEL, |_| {});
    let bytes = native_journal();
    // Every possible partial last row, empty startup, and the full ready/close
    // prefixes must remain counted even beside a well-shaped terminal JSON.
    for length in [0, bytes.len() - 208, bytes.len() - 104]
        .into_iter()
        .chain((1..104).map(|tail| bytes.len() - 104 + tail))
    {
        fixture.write(JOURNAL, &bytes[..length]);
        assert_eq!(
            fixture.census().unwrap().unresolved.len(),
            1,
            "length {length}"
        );
    }
    fixture.write(JOURNAL, bytes);
    // Mutate both sources consistently: their equality cannot expand the
    // actual keeper's one-second maximum or admit a zero/negative interval.
    for deadline in [999_999_999u64, 1_000_000_000, 2_000_000_001, u64::MAX] {
        let mut changed = bytes.to_vec();
        let stamp = changed.len() - 104 + 72;
        changed[stamp..stamp + 16].copy_from_slice(format!("{deadline:016x}").as_bytes());
        fixture.write(JOURNAL, &changed);
        fixture.terminal(LABEL, |row| {
            row["guard"]["readback"]["deadline_ns"] = serde_json::json!(deadline);
        });
        assert_eq!(
            fixture.census().unwrap().unresolved.len(),
            1,
            "deadline {deadline}"
        );
    }
    fixture.write(JOURNAL, bytes);
    for pointer in [
        "/guard/incarnation",
        "/guard/terminal/record_ordinal",
        "/guard/terminal/proof_sequence",
        "/guard/readback/closed_ns",
        "/guard/readback/deadline_ns",
    ] {
        fixture.terminal(LABEL, |row| {
            *row.pointer_mut(pointer).unwrap() = serde_json::json!(999)
        });
        assert_eq!(fixture.census().unwrap().unresolved.len(), 1, "{pointer}");
    }
    fixture.terminal(LABEL, |_| {});
    std::fs::remove_file(fixture.receipts.join(format!("{LABEL}.stderr.log"))).unwrap();
    assert_eq!(
        fixture.census().unwrap().unresolved.len(),
        1,
        "missing role"
    );
}

#[test]
fn malformed_keeper_rows_unknown_files_and_incarnation_collisions_refuse() {
    let fixture = Fixture::new();
    fixture.terminal(LABEL, |_| {});
    for offset in [0, 8, 16, 32, 40, 48, 52, 68] {
        let mut bytes = native_journal().to_vec();
        bytes[104 + offset] ^= 0x80;
        fixture.write(JOURNAL, &bytes);
        assert!(fixture.census().is_err(), "changed record offset {offset}");
    }
    fixture.write(JOURNAL, native_journal());
    for name in [
        "unrecognized",
        "ugb1-0000000000000000",
        "ugb1-000000000000000G",
        "ugb1-0000000000000007.extra",
    ] {
        fixture.write(name, b"");
        assert!(fixture.census().is_err(), "name {name}");
        std::fs::remove_file(fixture.receipts.join(name)).unwrap();
    }
    fixture.terminal("guard-b1-0000000000000007000000000000000a", |_| {});
    assert!(
        fixture
            .census()
            .unwrap_err()
            .to_string()
            .contains("ambiguous CLI identities")
    );
}

#[test]
fn keeper_file_authentication_and_census_bind_actual_inode_and_bytes() {
    let fixture = Fixture::new();
    let path = fixture.receipts.join(JOURNAL);
    fixture.write(JOURNAL, native_journal());
    let before = fixture.census().unwrap();
    let mut changed = native_journal().to_vec();
    // A syntactically valid field change still invalidates the double census.
    changed[104 + 24] ^= 1;
    fixture.write(JOURNAL, &changed);
    assert_ne!(before, fixture.census().unwrap());
    fixture.write(JOURNAL, native_journal());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(fixture.census().is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = fixture._temp.path().join("hardlink");
    std::fs::hard_link(&path, &link).unwrap();
    assert!(fixture.census().is_err());
    std::fs::remove_file(link).unwrap();
    assert!(
        guard_keeper_journal(
            fixture.receipt_fd.as_fd(),
            JOURNAL,
            INCARNATION,
            unsafe { libc::getuid() }.wrapping_add(1)
        )
        .is_err()
    );
    fixture.write(JOURNAL, &vec![0; 65_537]);
    assert!(fixture.census().is_err());
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("missing", &path).unwrap();
    assert!(fixture.census().is_err());
    std::fs::remove_file(&path).unwrap();
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(
        fixture.census().is_err(),
        "nonblocking open refuses FIFO without waiting for a writer"
    );
}

#[test]
fn journal_receipt_and_pin_count_once_and_keep_the_eight_launch_bound() {
    let fixture = Fixture::new();
    fixture.write(JOURNAL, &native_journal()[..104]);
    fixture.terminal(LABEL, |_| {});
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(fixture.pins.join(JOURNAL))
        .unwrap();
    for incarnation in 8..15u64 {
        fixture.write(&format!("ugb1-{incarnation:016x}"), b"");
    }
    let before = fixture.census().unwrap();
    assert_eq!(before.unresolved.len(), MAX_UNRESOLVED_GUARD_LAUNCHES);
    assert_eq!(MAX_UNRESOLVED_GUARD_LAUNCHES, 8);
    assert_eq!(fixture.census().unwrap(), before);
}
