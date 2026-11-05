use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::config::{load_typed_json_with_post_inspect_hook, ShadowConfig};
use crate::evidence::EvidenceBundle;
use crate::index::{
    read_decision, read_decision_at, read_pressure_plan, read_pressure_plan_at,
    rebuild_index_with_postpublication_hook, rebuild_index_with_prepublication_hook,
    rebuild_policy_index,
};
use crate::policy;
use crate::policy::Verdict;
use crate::replay::{
    replay_stream, replay_with_post_count_hook, PendingOperationKind, MAX_EVENT_BYTES,
    MAX_JSON_CONTAINER_DEPTH,
};
use crate::schema::parse_timestamp;
use crate::{canonical_sha256, read_index, rebuild_index, replay};

static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A `python3` command for driving the Python wrkslots oracle. These tests
/// exercise plain-worktree projects; new Python projects otherwise default to
/// disk-image slots, whose mounts the scratch-directory cleanup cannot remove.
fn python_command() -> Command {
    let mut command = Command::new("python3");
    command.env("WRKSLOTS_INIT_REPRESENTATION", "worktree");
    command
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let sequence = SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("wrkslotsd-test-{}-{sequence}", std::process::id()));
        fs::create_dir(&path).expect("create scratch directory");
        Self(path)
    }

    fn events(&self) -> PathBuf {
        let path = self.0.join("project/worktrees/EVENTS.node-a");
        fs::create_dir_all(path.parent().expect("event parent")).expect("create control directory");
        fs::create_dir(&path).expect("create event directory");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove scratch directory");
    }
}

fn active_record(slot: &str, generation: u64) -> Value {
    active_record_for(slot, &format!("agent-{slot}"), generation, "agent")
}

fn identity(pid: u64) -> Value {
    json!({
        "pid": pid,
        "start_ticks": pid + 100,
        "boot_id": "11111111-2222-3333-4444-555555555555",
        "host_id": "neutral-host",
        "cgroup_path": "/neutral.slice/worker.scope",
    })
}

fn checkout_for(slot: &str, slot_type: &str) -> Value {
    let root = if slot_type == "validate" {
        "worktrees/validate"
    } else {
        "worktrees/slots"
    };
    json!({
        "name": "source",
        "path": format!("{root}/{slot}/source"),
        "repository": ".",
        "branch": "topic",
        "start_point": "1".repeat(40),
        "remote": "origin",
        "remote_url_sha256": "2".repeat(64),
        "landed_ref": "refs/remotes/origin/main",
        "head": "3".repeat(40),
        "containing_remote_refs": ["refs/remotes/origin/topic"],
        "vcs": "git",
    })
}

fn active_record_for(slot: &str, agent: &str, generation: u64, slot_type: &str) -> Value {
    json!({
        "slot": slot,
        "agent": agent,
        "task": "exercise observer replay",
        "purpose": "neutral fixture",
        "slot_type": slot_type,
        "machine": "node-a",
        "generation": generation,
        "created_at": "2026-09-22T09:00:00+00:00",
        "heartbeat_at": "2026-09-22T09:30:00+00:00",
        "heartbeat_ttl_seconds": 600,
        "owner": identity(1000 + generation),
        "coordinator_lease": identity(2000 + generation),
        "coordinator_recovery_note": null,
        "handoff": {
            "recorded_at": "2026-09-22T09:34:00+00:00",
            "validation": ["neutral handoff validation"],
            "limitations": [],
            "continuation": "resume the neutral fixture",
        },
        "checkouts": [checkout_for(slot, slot_type)],
    })
}

/// The journal a create or import writer embeds for `record`, as far as replay
/// reads it: the slot type and every checkout path the attempt places.
fn attempt_journal(operation: &str, record: &Value) -> Value {
    if operation == "import-existing" {
        return json!({
            "schema": 2,
            "kind": operation,
            "machine": "node-a",
            "slot": record["slot"].clone(),
            "record": record.clone(),
        });
    }
    let planned = record["checkouts"]
        .as_array()
        .expect("fixture checkouts")
        .iter()
        .map(|checkout| json!({"name": checkout["name"].clone(), "destination": checkout["path"].clone()}))
        .collect::<Vec<_>>();
    json!({
        "schema": 2,
        "kind": operation,
        "machine": "node-a",
        "slot": record["slot"].clone(),
        "slot_type": record["slot_type"].clone(),
        "planned": planned,
        "created": [],
    })
}

fn imported_active_record(slot: &str, agent: &str, generation: u64) -> Value {
    let mut record = active_record_for(slot, agent, generation, "agent");
    let owner = record["owner"].clone();
    let row = json!({
        "status": "active",
        "allocated": record["created_at"].clone(),
        "task": record["task"].clone(),
        "purpose": record["purpose"].clone(),
        "owner_sidecar": {
            "supervisor_pid": owner["pid"].clone(),
            "start_ticks": owner["start_ticks"].clone(),
            "boot_id": owner["boot_id"].clone(),
            "cgroup_path": owner["cgroup_path"].clone(),
            "slot": slot,
            "agent": agent,
            "task": record["task"].clone(),
        },
        "agents": [{
            "name": agent,
            "read_only": false,
            "task": record["task"].clone(),
        }],
        "source_path": record["checkouts"][0]["path"].clone(),
    });
    let row_sha256 = canonical_sha256(&row).expect("historical row digest");
    record["layout"] = Value::String("nested".to_owned());
    record["import_source"] = json!({
        "format": "worktree-state-v3",
        "path": "legacy/state.json",
        "file_sha256": "4".repeat(64),
        "row_sha256": row_sha256,
        "status": "active",
        "row": row,
    });
    record
}

fn archive_record(slot: &str, generation: u64) -> Value {
    archive_record_for(slot, generation, "agent")
}

fn archive_record_for(slot: &str, generation: u64, slot_type: &str) -> Value {
    let finished_at = "2026-09-22T09:34:00+00:00";
    json!({
        "archive_id": format!("node-a:{slot}:{generation}:{finished_at}"),
        "slot": slot,
        "agent": format!("agent-{slot}"),
        "task": "exercise observer replay",
        "purpose": "neutral fixture",
        "slot_type": slot_type,
        "machine": "node-a",
        "generation": generation,
        "created_at": "2026-09-22T09:00:00+00:00",
        "finished_at": finished_at,
        "mode": "remove",
        "actor": "coordinator",
        "physical_storage": "removed",
        "validation": ["neutral validation evidence"],
        "limitations": [],
        "continuation": "continue with the next neutral fixture",
        "checkouts": [checkout_for(slot, slot_type)],
        "salvage": [{"kind": "neutral-evidence"}],
    })
}

fn imported_state(
    active: Vec<Value>,
    active_revision: u64,
    archive: Vec<Value>,
    archive_revision: u64,
) -> Value {
    json!({
        "active": {
            "schema": 2,
            "machine": "node-a",
            "revision": active_revision,
            "slots": active,
        },
        "archive": {
            "schema": 2,
            "machine": "node-a",
            "revision": archive_revision,
            "records": archive,
        },
        "holds": [],
    })
}

fn append_event(
    directory: &Path,
    sequence: u64,
    previous_sha256: &str,
    kind: &str,
    payload: Value,
) -> String {
    let core = json!({
        "schema": 1,
        "machine": "node-a",
        "sequence": sequence,
        "previous_sha256": previous_sha256,
        "recorded_at": format!("2026-09-22T09:35:{sequence:02}+00:00"),
        "kind": kind,
        "payload": payload,
    });
    let digest = canonical_sha256(&core).expect("canonical event digest");
    let mut event = core.as_object().expect("event object").clone();
    event.insert("sha256".to_owned(), Value::String(digest.clone()));
    let path = directory.join(format!("{sequence:020}.json"));
    fs::write(
        path,
        serde_json::to_vec_pretty(&Value::Object(event)).expect("encode event"),
    )
    .expect("write event");
    digest
}

fn one_event_log(directory: &Path) -> String {
    append_event(
        directory,
        1,
        &"0".repeat(64),
        "state-imported",
        imported_state(vec![active_record("slot-a", 1)], 3, vec![], 7),
    )
}

fn import_log(directory: &Path, active: Vec<Value>, archive: Vec<Value>) -> String {
    append_event(
        directory,
        1,
        &"0".repeat(64),
        "state-imported",
        imported_state(active, 0, archive, 0),
    )
}

const PYTHON_EVENT_ORACLE_PRELUDE: &str = r#"
import json
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from wrkslots import cli as w
root = Path(sys.argv[2])
config = w.Config(
    root=root,
    config_path=root / '.wrkslots.yml',
    worktrees=root / 'worktrees' / 'slots',
    control=root / 'worktrees',
    machine='node-a',
    default_remote='origin',
    default_landed_ref='refs/remotes/origin/main',
    heartbeat_ttl_seconds=600,
    liveness_command=Path('/bin/true'),
    layout='nested',
)
"#;

fn python_event_oracle(events: &Path, body: &str) -> std::process::Output {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repository root");
    let project = events
        .parent()
        .and_then(Path::parent)
        .expect("fixture project root");
    python_command()
        .arg("-c")
        .arg(format!("{PYTHON_EVENT_ORACLE_PRELUDE}{body}"))
        .arg(repository.join("py"))
        .arg(project)
        .output()
        .expect("run Python differential oracle")
}

fn python_replay(events: &Path) -> std::process::Output {
    python_event_oracle(
        events,
        r#"
active, archive = w._states_from_events(config, 'node-a', require_repository=False)
events = w._load_events(config, 'node-a')
print(json.dumps({
    'machine': active.machine,
    'replay_count': len(events),
    'tip_sha256': events[-1]['sha256'],
    'active_revision': active.revision,
    'active_records': w._active_to_obj(active)['slots'],
    'archive_revision': archive.revision,
    'archive_records': w._archive_to_obj(archive)['records'],
}, sort_keys=True, separators=(',', ':')))
"#,
    )
}

/// Python's pending-journal view: journal path name to `(slot, kind)`.
fn python_pending_journals(events: &Path) -> Value {
    let output = python_event_oracle(
        events,
        r#"
pending = w._pending_operations_from_events(config, 'node-a')
print(json.dumps({
    path.name: [journal.get('slot'), journal.get('kind')]
    for path, journal in pending.items()
}, sort_keys=True))
"#,
    );
    assert!(
        output.status.success(),
        "Python pending-journal oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse Python pending journals")
}

fn python_synthetic_audit_verdict(events: &Path) -> std::process::Output {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repository root");
    let project = events
        .parent()
        .and_then(Path::parent)
        .expect("fixture project root");
    let script = r#"
import json
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from wrkslots import cli as w
root = Path(sys.argv[2])
config = w.Config(
    root=root,
    config_path=root / '.wrkslots.yml',
    worktrees=root / 'worktrees' / 'slots',
    control=root / 'worktrees',
    machine='node-a',
    default_remote='origin',
    default_landed_ref='refs/remotes/origin/main',
    heartbeat_ttl_seconds=600,
    liveness_command=Path('/bin/true'),
    layout='nested',
)
active, _ = w._states_from_events(config, 'node-a', require_repository=False)
record = active.slots[0]
events = w._load_events(config, 'node-a')
w._process_state = lambda _identity: ('dead', 'synthetic dead owner')
w._registered_liveness_state = lambda _config, _record: ('dead', 'synthetic dead probe')
w._heartbeat_diagnosis = lambda _record: (601.0, True)
w._assert_record_paths = lambda *_args, **_kwargs: None
w._assert_slot_contents = lambda *_args, **_kwargs: None
w._assert_handoff_read = lambda *_args, **_kwargs: None
w._assert_slot_unused = lambda *_args, **_kwargs: None
w._assert_cache_policy_untracked = lambda *_args, **_kwargs: None
w._submodule_salvage_checkouts = lambda *_args, **_kwargs: ()
w._cache_directories = lambda *_args, **_kwargs: ()
class Vcs:
    def verify_existing_worktree(self, _repository, _path): return record.checkouts[0].head
    def branch(self, _path): return record.checkouts[0].branch
    def operation_paths(self, _path): return ()
    def assert_ordinary_history(self, _path): return None
    def assert_ordinary_index(self, _path): return None
    def remote_url_sha256(self, _path, _remote): return record.checkouts[0].remote_url_sha256
row, _running = w._audit_record(
    config, record, events=events, vcs=Vcs(), process_census=object()
)
print(row['verdict'])
"#;
    python_command()
        .arg("-c")
        .arg(script)
        .arg(repository.join("py"))
        .arg(project)
        .output()
        .expect("run Python synthetic audit oracle")
}

fn python_interrupted_finish(root: &Path) -> std::process::Output {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repository root");
    let script = r#"
import hashlib
import json
import os
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
sys.path.insert(0, sys.argv[2])
from wrkslots import cli as w
import test_lifecycle as t
os.environ['WRKSLOTS_MACHINE'] = 'testhost'
root = Path(sys.argv[3])
project, _repository, _remote = t.make_project(root)
created = t.create(project)
assert created.returncode == 0, created.stderr
finished = t.finish(project)
assert finished.returncode == 0, finished.stderr
t.mark_owner_dead(project)
t.set_liveness(project, 'dead')
class Interrupted(RuntimeError):
    pass
def interrupt(point):
    if point == 'after-finish-journal':
        raise Interrupted()
w._interrupt_for_test = interrupt
w._assert_slot_unused = lambda *_args, **_kwargs: None
try:
    w.main([
        '--project-root', str(project), 'remove', 'slot01',
        '--coordinator-pid', str(os.getpid()), '--expected-generation', '1',
        '--coordinator-authorized',
    ])
except Interrupted:
    pass
else:
    raise AssertionError('remove did not stop after its finish journal')
config = w._load_config(str(project), 'testhost')
pending = w._pending_operations_from_events(config, 'testhost')
assert len(pending) == 1, pending
path, journal = next(iter(pending.items()))
canonical = json.dumps(journal, sort_keys=True, separators=(',', ':'), ensure_ascii=True)
print(json.dumps({
    'events': str(config.control / 'EVENTS.testhost'),
    'path': path.name,
    'kind': journal['kind'],
    'slot': journal['slot'],
    'generation': journal['record']['generation'],
    'phase': journal['phase'],
    'journal_sha256': hashlib.sha256(canonical.encode('utf-8')).hexdigest(),
}, sort_keys=True, separators=(',', ':')))
"#;
    python_command()
        .arg("-c")
        .arg(script)
        .arg(repository.join("py"))
        .arg(repository.join("py/wrkslots/tests"))
        .arg(root)
        .output()
        .expect("run interrupted Python finish fixture")
}

fn python_interrupted_create(root: &Path) -> std::process::Output {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repository root");
    let script = r#"
import json
import os
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
sys.path.insert(0, sys.argv[2])
from wrkslots import cli as w
import test_lifecycle as t
os.environ['WRKSLOTS_MACHINE'] = 'testhost'
root = Path(sys.argv[3])
project, _repository, _remote = t.make_project(root)
created = t.create(project, env={'WRKSLOTS_TEST_INTERRUPT': 'after-create-worktree'})
assert created.returncode == 86, created.stderr
config = w._load_config(str(project), 'testhost')
pending = w._pending_operations_from_events(config, 'testhost')
assert len(pending) == 1, pending
path, journal = next(iter(pending.items()))
print(json.dumps({
    'events': str(config.control / 'EVENTS.testhost'),
    'path': path.name,
    'kind': journal['kind'],
    'slot': journal['slot'],
}, sort_keys=True, separators=(',', ':')))
"#;
    python_command()
        .arg("-c")
        .arg(script)
        .arg(repository.join("py"))
        .arg(repository.join("py/wrkslots/tests"))
        .arg(root)
        .output()
        .expect("run interrupted Python create fixture")
}

fn python_late_refusal_after_journal_completion(root: &Path, mode: &str) -> std::process::Output {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repository root");
    let script = r#"
import contextlib
import io
import json
import os
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
sys.path.insert(0, sys.argv[2])
from wrkslots import cli as w
import test_lifecycle as t
os.environ['WRKSLOTS_MACHINE'] = 'testhost'
root = Path(sys.argv[3])
mode = sys.argv[4]
project, _repository, _remote = t.make_project(root, cache_globs=('target',))
created = t.create(project)
assert created.returncode == 0, created.stderr
if mode == 'retirement':
    handoff = t.checkout(project).parent / 'HANDOFF.md'
    handoff.write_text('queue this slot\n', encoding='utf-8')
    read = t.raw_command(
        project, 'read-handoff', 'slot01', '--coordinator-pid', str(os.getpid())
    )
    assert read.returncode == 0, read.stderr
finished = t.finish(project)
assert finished.returncode == 0, finished.stderr
t.mark_owner_dead(project)
t.set_liveness(project, 'dead')
w._assert_slot_unused = lambda *_args, **_kwargs: None
class Interrupted(RuntimeError):
    pass
def interrupt(point):
    if point == 'after-finish-journal':
        if mode == 'retirement':
            (t.checkout(project) / 'late-source.txt').write_text(
                'appeared after salvage\n', encoding='utf-8'
            )
        else:
            nested = t.checkout(project) / 'target' / 'nested' / '.git'
            nested.mkdir(parents=True)
            (nested / 'HEAD').write_text('ref: refs/heads/main\n', encoding='utf-8')
            raise Interrupted()
w._interrupt_for_test = interrupt
if mode == 'retirement':
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
        result = w.main([
            '--project-root', str(project), 'retire-pending', '--limit', '1',
            '--coordinator-pid', str(os.getpid()), '--format', 'json',
        ])
    assert result == 0, result
else:
    try:
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            w.main([
                '--project-root', str(project), 'remove', 'slot01',
                '--coordinator-pid', str(os.getpid()), '--expected-generation', '1',
                '--coordinator-authorized',
            ])
    except Interrupted:
        pass
    else:
        raise AssertionError('remove did not stop after publishing its finish journal')
    w._interrupt_for_test = lambda _point: None
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
        recovered = w.main([
            '--project-root', str(project), 'recover', '--coordinator-pid', str(os.getpid()),
            '--coordinator-authorized',
        ])
    assert recovered == 3, recovered
config = w._load_config(str(project), 'testhost')
events = w._load_events(config, 'testhost')
kinds = [event['kind'] for event in events]
assert 'reclaim-started' in kinds, kinds
assert 'operation-completed' in kinds, kinds
if mode == 'retirement':
    assert 'retirement-attempted' in kinds, kinds
    assert 'recovery-started' not in kinds, kinds
else:
    assert 'retirement-attempted' not in kinds, kinds
    assert 'recovery-started' in kinds, kinds
assert not w._outstanding_journals(config)
active = w._load_active(config, require_repository=False)
archive = w._load_archive(config, require_repository=False)
assert [record.slot for record in active.slots] == ['slot01']
assert not archive.records
print(json.dumps({
    'events': str(config.control / 'EVENTS.testhost'),
    'event_kinds': kinds,
}, sort_keys=True, separators=(',', ':')))
"#;
    python_command()
        .arg("-c")
        .arg(script)
        .arg(repository.join("py"))
        .arg(repository.join("py/wrkslots/tests"))
        .arg(root)
        .arg(mode)
        .output()
        .expect("run Python late-refusal fixture")
}

fn python_scoped_journal_retry(root: &Path, operation: &str) -> std::process::Output {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repository root");
    let script = r#"
import contextlib
import io
import json
import os
import shutil
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
sys.path.insert(0, sys.argv[2])
from wrkslots import cli as w
import test_lifecycle as t
os.environ['WRKSLOTS_MACHINE'] = 'testhost'
root = Path(sys.argv[3])
operation = sys.argv[4]

if operation == 'create':
    project, _repository, _remote = t.make_project(
        root, post_provision_hooks=('exit 9',)
    )
    first = t.create(project)
    assert first.returncode == 3, first.stderr
    aborted = t.command(
        project, 'recover', '--coordinator-pid', str(os.getpid()), '--abort-create'
    )
    assert aborted.returncode == 0, aborted.stderr
    retried = t.create(project)
    assert retried.returncode == 3, retried.stderr
else:
    assert operation == 'finish'
    project, _repository, _remote = t.make_project(root, cache_globs=('target',))
    created = t.create(project)
    assert created.returncode == 0, created.stderr
    finished = t.finish(project)
    assert finished.returncode == 0, finished.stderr
    t.mark_owner_dead(project)
    t.set_liveness(project, 'dead')
    w._assert_slot_unused = lambda *_args, **_kwargs: None

    class Interrupted(RuntimeError):
        pass

    def refuse_after_journal(point):
        if point == 'after-finish-journal':
            nested = t.checkout(project) / 'target' / 'nested' / '.git'
            nested.mkdir(parents=True)
            (nested / 'HEAD').write_text('ref: refs/heads/main\n', encoding='utf-8')
            raise Interrupted()

    w._interrupt_for_test = refuse_after_journal
    try:
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            w.main([
                '--project-root', str(project), 'remove', 'slot01',
                '--coordinator-pid', str(os.getpid()), '--expected-generation', '1',
                '--coordinator-authorized',
            ])
    except Interrupted:
        pass
    else:
        raise AssertionError('first finish did not stop after publishing its journal')
    w._interrupt_for_test = lambda _point: None
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
        refused = w.main([
            '--project-root', str(project), 'recover', '--coordinator-pid', str(os.getpid()),
            '--coordinator-authorized',
        ])
    assert refused == 3, refused
    assert not w._outstanding_journals(w._load_config(str(project), 'testhost'))
    shutil.rmtree(t.checkout(project) / 'target')

    def stop_retry(point):
        if point == 'after-finish-journal':
            raise Interrupted()

    w._interrupt_for_test = stop_retry
    try:
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            w.main([
                '--project-root', str(project), 'remove', 'slot01',
                '--coordinator-pid', str(os.getpid()), '--expected-generation', '1',
                '--coordinator-authorized',
            ])
    except Interrupted:
        pass
    else:
        raise AssertionError('finish retry did not publish a new journal')

config = w._load_config(str(project), 'testhost')
pending = w._pending_operations_from_events(config, 'testhost')
assert len(pending) == 1, pending
path, journal = next(iter(pending.items()))
events = w._load_events(config, 'testhost')
progress = [event for event in events if event['kind'] == 'operation-progress-recorded']
completed = [event for event in events if event['kind'] == 'operation-completed']
assert len(progress) >= 2, progress
assert completed, events
assert progress[-1]['payload']['journal_path'] == completed[-1]['payload']['journal_path']
print(json.dumps({
    'events': str(config.control / 'EVENTS.testhost'),
    'path': path.name,
    'kind': journal['kind'],
    'slot': journal['slot'],
}, sort_keys=True, separators=(',', ':')))
"#;
    python_command()
        .arg("-c")
        .arg(script)
        .arg(repository.join("py"))
        .arg(repository.join("py/wrkslots/tests"))
        .arg(root)
        .arg(operation)
        .output()
        .expect("run Python scoped-journal retry fixture")
}

fn python_canonical(source: &str) -> (String, String) {
    let script = r#"
import hashlib
import json
import sys
canonical = json.dumps(
    json.loads(sys.argv[1]), sort_keys=True, separators=(',', ':'), ensure_ascii=True
)
print(json.dumps({
    'canonical': canonical,
    'sha256': hashlib.sha256(canonical.encode('utf-8')).hexdigest(),
}))
"#;
    let output = python_command()
        .arg("-c")
        .arg(script)
        .arg(source)
        .output()
        .expect("run Python canonical JSON oracle");
    assert!(
        output.status.success(),
        "Python canonical JSON oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value =
        serde_json::from_slice(&output.stdout).expect("parse Python oracle result");
    (
        response["canonical"]
            .as_str()
            .expect("Python canonical JSON")
            .to_owned(),
        response["sha256"]
            .as_str()
            .expect("Python canonical digest")
            .to_owned(),
    )
}

fn python_timestamp_instants(values: &[String]) -> Vec<i64> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repository root");
    let script = r#"
import datetime as dt
import json
import sys
sys.path.insert(0, sys.argv[1])
from wrkslots import cli as w
epoch = dt.datetime(1970, 1, 1, tzinfo=dt.timezone.utc)
print(json.dumps([
    (w._parse_timestamp(value, 'Rust differential fixture') - epoch)
    // dt.timedelta(microseconds=1)
    for value in json.load(sys.stdin)
], separators=(',', ':')))
"#;
    let encoded = serde_json::to_string(values).expect("encode timestamp instants");
    let mut child = python_command()
        .arg("-c")
        .arg(script)
        .arg(repository.join("py"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start Python timestamp instant oracle");
    child
        .stdin
        .take()
        .expect("Python timestamp instant oracle stdin")
        .write_all(encoded.as_bytes())
        .expect("write Python timestamp instants");
    let output = child
        .wait_with_output()
        .expect("wait for Python timestamp instant oracle");
    assert!(
        output.status.success(),
        "Python timestamp instant oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("decode Python timestamp instants")
}

fn python_timestamp_acceptance(values: &[String]) -> Vec<bool> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repository root");
    let script = r#"
import json
import sys
sys.path.insert(0, sys.argv[1])
from wrkslots import cli as w
results = []
for value in json.load(sys.stdin):
    try:
        w._parse_timestamp(value, 'Rust differential fixture')
    except w.StateError:
        results.append(False)
    else:
        results.append(True)
print(json.dumps(results, separators=(',', ':')))
"#;
    let encoded = serde_json::to_string(values).expect("encode timestamp matrix");
    let mut child = python_command()
        .arg("-c")
        .arg(script)
        .arg(repository.join("py"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start Python timestamp oracle");
    child
        .stdin
        .take()
        .expect("Python timestamp oracle stdin")
        .write_all(encoded.as_bytes())
        .expect("write Python timestamp matrix");
    let output = child
        .wait_with_output()
        .expect("run Python timestamp oracle");
    assert!(
        output.status.success(),
        "Python timestamp oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse Python timestamp oracle result")
}

fn rewrite_first_event_timestamp_with_python_hash(events: &Path, timestamp: &str) {
    let path = events.join("00000000000000000001.json");
    let mut event: Value =
        serde_json::from_slice(&fs::read(&path).expect("read event")).expect("parse event");
    event["recorded_at"] = Value::String(timestamp.to_owned());
    let mut core = event.as_object().expect("event object").clone();
    core.remove("sha256");
    let source = serde_json::to_string(&Value::Object(core)).expect("encode event core");
    let (_, digest) = python_canonical(&source);
    event["sha256"] = Value::String(digest);
    fs::write(
        path,
        serde_json::to_vec_pretty(&event).expect("encode timestamp event"),
    )
    .expect("write timestamp event");
}

fn append_python_hashed_payload(
    directory: &Path,
    sequence: u64,
    previous_sha256: &str,
    payload: &str,
) -> String {
    let core = raw_event_core(sequence, previous_sha256, payload);
    let (_, digest) = python_canonical(&core);
    write_raw_event(directory, sequence, &core, &digest);
    digest
}

fn raw_event_core(sequence: u64, previous_sha256: &str, payload: &str) -> String {
    format!(
        "{{\"schema\":1,\"machine\":\"node-a\",\"sequence\":{sequence},\"previous_sha256\":\"{previous_sha256}\",\"recorded_at\":\"2026-09-22T09:35:{sequence:02}+00:00\",\"kind\":\"handoff-written\",\"payload\":{payload}}}"
    )
}

fn write_raw_event(directory: &Path, sequence: u64, core: &str, digest: &str) {
    let event = format!(
        "{},\"sha256\":\"{digest}\"}}\n",
        core.strip_suffix('}').expect("event core object")
    );
    fs::write(
        directory.join(format!("{sequence:020}.json")),
        event.as_bytes(),
    )
    .expect("write Python-hashed event");
}

fn nested_payload(depth: usize) -> String {
    format!(
        "{{\"nested\":{}null{}}}",
        "[".repeat(depth),
        "]".repeat(depth)
    )
}

#[test]
fn valid_current_event_shapes_replay_to_state_summary() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let active_a = active_record("slot-a", 1);
    let first = append_event(
        &events,
        1,
        &"0".repeat(64),
        "state-imported",
        imported_state(
            vec![active_a.clone()],
            3,
            vec![archive_record("slot-old", 4)],
            7,
        ),
    );
    let second = append_event(
        &events,
        2,
        &first,
        "operation-progress-recorded",
        json!({
            "slot": "slot-a",
            "operation": "finish",
            "journal_path": "FINISH.6.node-a.6.slot-a.journal",
            "journal": {
                "schema": 2,
                "kind": "finish",
                "machine": "node-a",
                "slot": "slot-a"
            }
        }),
    );
    let third = append_event(
        &events,
        3,
        &second,
        "archive-state-recorded",
        json!({
            "action": "finished",
            "slot": "slot-a",
            "previous_revision": 7,
            "revision": 8,
            "record": archive_record("slot-a", 1),
            "evidence": {},
        }),
    );
    let fourth = append_event(
        &events,
        4,
        &third,
        "active-state-recorded",
        json!({
            "action": "finished",
            "slot": "slot-a",
            "previous_revision": 3,
            "revision": 4,
            "previous_record_sha256": canonical_sha256(&active_a).expect("record digest"),
            "record": null,
            "evidence": {},
        }),
    );
    let fifth = append_event(
        &events,
        5,
        &fourth,
        "operation-completed",
        json!({
            "slot": "slot-a",
            "operation": "finish",
            "journal_path": "FINISH.6.node-a.6.slot-a.journal",
        }),
    );
    let tip = append_event(
        &events,
        6,
        &fifth,
        "active-state-recorded",
        json!({
            "action": "allocated",
            "slot": "slot-b",
            "previous_revision": 4,
            "revision": 5,
            "previous_record_sha256": null,
            "record": active_record("slot-b", 2),
            "evidence": {},
        }),
    );
    let summary = replay(&events).expect("valid replay");
    assert_eq!(summary.machine, "node-a");
    assert_eq!(summary.replay_count, 6);
    assert_eq!(summary.tip_sha256, tip);
    assert_eq!(summary.active_revision, 5);
    assert_eq!(summary.active_count, 1);
    assert_eq!(summary.archive_revision, 8);
    assert_eq!(summary.archive_count, 2);
}

#[test]
fn chain_digest_sequence_and_filename_corruption_are_each_rejected() {
    let chain_scratch = Scratch::new();
    let chain_events = chain_scratch.events();
    one_event_log(&chain_events);
    append_event(
        &chain_events,
        2,
        &"f".repeat(64),
        "handoff-written",
        json!({}),
    );
    assert!(replay(&chain_events)
        .expect_err("broken chain must fail")
        .to_string()
        .contains("hash chain is broken"));

    let digest_scratch = Scratch::new();
    let digest_events = digest_scratch.events();
    one_event_log(&digest_events);
    let path = digest_events.join("00000000000000000001.json");
    let mut event: Value =
        serde_json::from_slice(&fs::read(&path).expect("read event")).expect("parse event");
    event["sha256"] = Value::String("f".repeat(64));
    fs::write(
        &path,
        serde_json::to_vec_pretty(&event).expect("encode damaged event"),
    )
    .expect("write damaged event");
    assert!(replay(&digest_events)
        .expect_err("digest mismatch must fail")
        .to_string()
        .contains("digest does not match"));

    let sequence_scratch = Scratch::new();
    let sequence_events = sequence_scratch.events();
    let first = one_event_log(&sequence_events);
    append_event(&sequence_events, 3, &first, "handoff-written", json!({}));
    assert!(replay(&sequence_events)
        .expect_err("sequence gap must fail")
        .to_string()
        .contains("sequence gap"));

    let filename_scratch = Scratch::new();
    let filename_events = filename_scratch.events();
    let first = one_event_log(&filename_events);
    append_event(&filename_events, 3, &first, "handoff-written", json!({}));
    fs::rename(
        filename_events.join("00000000000000000003.json"),
        filename_events.join("00000000000000000002.json"),
    )
    .expect("rename event to mismatched filename");
    assert!(replay(&filename_events)
        .expect_err("sequence/filename mismatch must fail")
        .to_string()
        .contains("sequence does not match its filename"));
}

#[test]
fn canonical_hash_matches_ascii_escaped_python_unicode() {
    let value = json!({
        "emoji": "🦀",
        "latin": "café",
        "line": "a\nb",
        "cjk": "雪",
    });
    assert_eq!(
        canonical_sha256(&value).expect("canonical digest"),
        "e6a8ac7ee8665bfdb1139b074bd1defbbe94f67eec3e22a273acc5ce5ed0016e"
    );
}

#[test]
fn canonical_hash_matches_python_float_rendering() {
    for (token, expected) in [
        (
            "1.0",
            "3b6b06ecd1c968c8e738e0f11c4bb361fca80a9a694de22fe66a05286afbd081",
        ),
        (
            "1e-7",
            "ff7a1315299260617fe404199e54e6d976a0b03e47da54fccec073c2fa48ff5c",
        ),
        (
            "1e-5",
            "a4aaff4cb2776bd827d3e6a14a54ccc534ca6b62e36579fc1c53b537c89752a4",
        ),
        (
            "1e16",
            "3b2cb112e050812d03ee42b0c83897293d662709d335a4f1ce7e59d29de23ac1",
        ),
        (
            "5e-324",
            "cf4df7d2834198d284947133d3ec7a66f3d94e8d447cac6a0783324211b36d43",
        ),
    ] {
        let number: Value = serde_json::from_str(token).expect("parse float fixture");
        let value = json!({"n": number});
        assert_eq!(canonical_sha256(&value).expect("float digest"), expected);
    }
}

#[test]
fn python_hashed_supported_number_domain_replays_and_indexes_exactly() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let first = one_event_log(&events);
    let payload = r#"{"numbers":[-9223372036854775808,18446744073709551615,-0,0,1.0,-0.0,-0e0,1.2300,1.234567890123456789,1E+06,1e-6,1e-324,-1e-324,5e-324,1.7976931348623157e308]}"#;
    let (expected_payload, expected_payload_digest) = python_canonical(payload);
    assert_eq!(
        expected_payload,
        r#"{"numbers":[-9223372036854775808,18446744073709551615,0,0,1.0,-0.0,-0.0,1.23,1.2345678901234567,1000000.0,1e-06,0.0,-0.0,5e-324,1.7976931348623157e+308]}"#
    );
    assert_eq!(
        expected_payload_digest,
        "94d699409d28b6f92bb4fa8d653ecc8cb5ecefdbd095c88551e6488bdc0f8dd3"
    );
    let tip = append_python_hashed_payload(&events, 2, &first, payload);

    let summary = replay(&events).expect("supported Python numbers replay");
    assert_eq!((summary.replay_count, summary.tip_sha256), (2, tip));
    let python = python_replay(&events);
    assert!(
        python.status.success(),
        "Python rejected supported numbers: {}",
        String::from_utf8_lossy(&python.stderr)
    );

    let index = scratch.0.join("numbers.sqlite");
    rebuild_index(&events, &index).expect("index supported Python numbers");
    let connection = Connection::open(&index).expect("open numeric index");
    let indexed_payload: String = connection
        .query_row(
            "SELECT payload_json FROM event_log WHERE sequence = 2",
            [],
            |row| row.get(0),
        )
        .expect("read indexed numeric payload");
    assert_eq!(indexed_payload, expected_payload);
}

#[test]
fn python_numbers_outside_observer_domain_fail_closed() {
    for (token, expected_error) in [
        ("18446744073709551616", "lossless observer domain"),
        ("-9223372036854775809", "lossless observer domain"),
        ("1e400", "non-finite number"),
        ("-1e400", "non-finite number"),
        ("NaN", "cannot parse event"),
        ("Infinity", "cannot parse event"),
        ("-Infinity", "cannot parse event"),
    ] {
        let scratch = Scratch::new();
        let events = scratch.events();
        let first = one_event_log(&events);
        append_python_hashed_payload(&events, 2, &first, &format!("{{\"number\":{token}}}"));
        let python = python_replay(&events);
        assert!(
            python.status.success(),
            "Python rejected its supported number {token}: {}",
            String::from_utf8_lossy(&python.stderr)
        );
        let error = replay(&events).expect_err("unsupported observer number must fail");
        assert!(
            error.to_string().contains(expected_error),
            "unexpected error for {token}: {error}"
        );
    }
}

#[test]
fn json_depth_boundary_matches_the_declared_observer_limit() {
    let accepted = Scratch::new();
    let accepted_events = accepted.events();
    let first = one_event_log(&accepted_events);
    let payload = nested_payload(MAX_JSON_CONTAINER_DEPTH - 2);
    append_python_hashed_payload(&accepted_events, 2, &first, &payload);
    replay(&accepted_events).expect("declared JSON nesting boundary must replay");
    assert!(python_replay(&accepted_events).status.success());

    let rejected = Scratch::new();
    let rejected_events = rejected.events();
    let first = one_event_log(&rejected_events);
    let payload = nested_payload(MAX_JSON_CONTAINER_DEPTH - 1);
    append_python_hashed_payload(&rejected_events, 2, &first, &payload);
    assert!(python_replay(&rejected_events).status.success());
    assert!(replay(&rejected_events)
        .expect_err("over-depth Python event must fail closed")
        .to_string()
        .contains("127-container JSON nesting limit"));
}

#[test]
fn unicode_scalar_pair_matches_python_but_lone_surrogate_fails_closed() {
    let raw = Scratch::new();
    let raw_events = raw.events();
    let first = one_event_log(&raw_events);
    let raw_tip = append_python_hashed_payload(&raw_events, 2, &first, r#"{"text":"🦀"}"#);
    replay(&raw_events).expect("raw Unicode scalar event");

    let escaped = Scratch::new();
    let escaped_events = escaped.events();
    let first = one_event_log(&escaped_events);
    let escaped_tip =
        append_python_hashed_payload(&escaped_events, 2, &first, r#"{"text":"\ud83e\udd80"}"#);
    replay(&escaped_events).expect("paired-surrogate Unicode scalar event");
    assert_eq!(raw_tip, escaped_tip);

    for payload in [
        r#"{"text":"bad\udc80byte"}"#,
        r#"{"text":"bad\ud800byte"}"#,
        r#"{"text":"bad\ud800\u0041byte"}"#,
    ] {
        let rejected = Scratch::new();
        let rejected_events = rejected.events();
        let first = one_event_log(&rejected_events);
        append_python_hashed_payload(&rejected_events, 2, &first, payload);
        assert!(python_replay(&rejected_events).status.success());
        assert!(replay(&rejected_events)
            .expect_err("lone surrogate must fail closed")
            .to_string()
            .contains("lone Unicode surrogate escape"));
    }

    let escaped_literal = Scratch::new();
    let escaped_literal_events = escaped_literal.events();
    let first = one_event_log(&escaped_literal_events);
    append_python_hashed_payload(&escaped_literal_events, 2, &first, r#"{"text":"\\ud800"}"#);
    replay(&escaped_literal_events).expect("escaped surrogate spelling is scalar-only text");
}

#[test]
fn malformed_numbers_are_reported_as_json_parse_errors() {
    for token in [
        "1e",
        "1e+",
        "1-2",
        "1e400x",
        "18446744073709551616x",
        "1e400 x",
        "18446744073709551616 x",
    ] {
        let scratch = Scratch::new();
        let events = scratch.events();
        let first = one_event_log(&events);
        let core = raw_event_core(2, &first, &format!("{{\"number\":{token}}}"));
        write_raw_event(&events, 2, &core, &"0".repeat(64));
        let error = replay(&events).expect_err("malformed JSON number must fail");
        assert!(
            error.to_string().contains("cannot parse event"),
            "malformed number {token} was mislabeled: {error}"
        );
    }
}

#[test]
fn event_timestamps_require_an_iso_timezone() {
    let valid = Scratch::new();
    let events = valid.events();
    one_event_log(&events);
    let path = events.join("00000000000000000001.json");
    let mut event: Value =
        serde_json::from_slice(&fs::read(&path).expect("read event")).expect("parse event");
    event["recorded_at"] = Value::String("2026-09-22 15:30:00+05:30".to_owned());
    let mut core = event.as_object().expect("event object").clone();
    core.remove("sha256");
    event["sha256"] = Value::String(
        canonical_sha256(&Value::Object(core)).expect("recompute timestamp event digest"),
    );
    fs::write(
        &path,
        serde_json::to_vec_pretty(&event).expect("encode event"),
    )
    .expect("write event");
    replay(&events).expect("timezone-aware ISO timestamp");

    event["recorded_at"] = Value::String("2026-09-22T10:00:00".to_owned());
    let mut core = event.as_object().expect("event object").clone();
    core.remove("sha256");
    event["sha256"] = Value::String(
        canonical_sha256(&Value::Object(core)).expect("recompute naive event digest"),
    );
    fs::write(
        &path,
        serde_json::to_vec_pretty(&event).expect("encode event"),
    )
    .expect("write event");
    assert!(replay(&events)
        .expect_err("timezone-naive event timestamp must fail")
        .to_string()
        .contains("timezone-naive"));
}

/// Return the generated timestamp matrix: every value Python accepts, then
/// every value it rejects, with the number of accepted values.
fn python_timestamp_matrix() -> (usize, Vec<String>) {
    let dates = [
        "2026-09-22",
        "20260922",
        "2026-W39-2",
        "2026W392",
        "2026-W39",
        "2026W39",
    ];
    let separators = ["T", " ", "x", "+", "🐍", "\u{301}"];
    let times = [
        "00",
        "23",
        "0000",
        "2359",
        "00:00",
        "23:59",
        "000000",
        "235959",
        "00:00:00",
        "23:59:59",
        "10.1",
        "10,1234567",
        "1000.5",
        "10:00,5",
        "100000.1234567",
        "10:00:00,123",
        "10.",
        "10,",
        "123",
        "12345",
        "1234567",
        "12345678",
        "12:",
        "12x",
        "12:34:",
        "12:34x",
        "12:34:56:",
        "12:34:56:7",
        "10.123456x",
        "10.123456🐍",
        "12:34:56.123456:garbage",
        "123456123456x",
        "12:34:56:123456x",
    ];
    let zones = [
        "Z",
        "+00",
        "-00",
        "+0000",
        "-00:00",
        "+000000",
        "+00:00:00",
        "+23:59:59.9999999",
        "-235959,9",
        "+00:99",
        "+0099",
        "+23:00:99",
        "+12345678",
        "-23275192",
        "+12:34:56:7",
        // CPython returns UTC and discards the fraction of a zero-second offset.
        "+00.5",
        "-00:00:00.5",
        "+00:00,9",
    ];
    let mut accepted = BTreeSet::new();
    for date in dates {
        for separator in separators {
            for time in times {
                for zone in zones {
                    accepted.insert(format!("{date}{separator}{time}{zone}"));
                }
            }
        }
    }
    accepted.insert("2026-09-22010:00:00+00".to_owned());
    accepted.insert("2026W392010:00:00+00".to_owned());
    accepted.insert("9999-W52-5T00Z".to_owned());

    let mut rejected = BTreeSet::new();
    for date in [
        "0000-01-01",
        "2026-02-29",
        "2026-09-31",
        "2026-13-01",
        "2026-W00-1",
        "2026-W54-1",
        "2026-W39-0",
        "2026-W39-8",
        "2026-266",
        "2026266",
        "2026-09",
        "9999-W52-6",
        "9999-W52-7",
        "9999W526",
        "9999W527",
    ] {
        rejected.insert(format!("{date}T10:00:00+00:00"));
    }
    rejected.insert("2026-W39-2010:00:00+00".to_owned());
    for time in [
        "0",
        "24",
        "2400",
        "24:00",
        "2360",
        "23:60",
        "235960",
        "23:59:60",
        "10:0000",
        "1000:00",
        "123456x5",
        "10.12345x",
        "12345612345x",
        "12:34:56:12345x",
    ] {
        rejected.insert(format!("2026-09-22T{time}+00:00"));
    }
    for zone in [
        "",
        "z",
        "+0",
        "+0:1",
        "+000",
        "+00:",
        "+00::00",
        "+0000:00",
        "+24",
        "+2400",
        "+23:60",
        "+235960",
        "+010203x5",
        "+01:02:03x5",
        "Z+00",
    ] {
        rejected.insert(format!("2026-09-22T10:00:00{zone}"));
    }
    assert!(accepted.is_disjoint(&rejected));

    let accepted_count = accepted.len();
    (
        accepted_count,
        accepted.into_iter().chain(rejected).collect::<Vec<_>>(),
    )
}

#[test]
fn timestamp_parser_matches_python_generated_matrix() {
    let (accepted_count, values) = python_timestamp_matrix();
    // 6 dates x 6 separators x 33 times x 18 zones, plus 3 explicit values.
    assert_eq!(accepted_count, 21_387);
    // The accepted values plus 45 explicit rejected values.
    assert_eq!(values.len(), 21_432);
    let python = python_timestamp_acceptance(&values);
    assert_eq!(python.len(), values.len());
    for (index, (value, python_accepts)) in values.iter().zip(python).enumerate() {
        let expected = index < accepted_count;
        assert_eq!(
            python_accepts, expected,
            "matrix classification disagrees with Python for {value:?}"
        );
        assert_eq!(
            parse_timestamp(value, "fixture timestamp").is_ok(),
            python_accepts,
            "Rust/Python timestamp mismatch for {value:?}"
        );
    }
}

#[test]
fn fully_hashed_event_timestamps_match_python_at_regression_boundaries() {
    for timestamp in [
        "2026-09-22T100000+00:00",
        "2026-09-22T1000+00:00",
        "2026-09-22T1000Z",
        "2026-09-22T123Z",
        "2026-09-22T12345+00:00",
        "20260922T10:00:00+00:00",
        "2026-W39-2🐍1000+00:00",
    ] {
        let scratch = Scratch::new();
        let events = scratch.events();
        one_event_log(&events);
        rewrite_first_event_timestamp_with_python_hash(&events, timestamp);
        let python = python_replay(&events);
        assert!(
            python.status.success(),
            "Python rejected valid fully-hashed event timestamp {timestamp:?}: {}",
            String::from_utf8_lossy(&python.stderr)
        );
        replay(&events)
            .unwrap_or_else(|error| panic!("Rust rejected event timestamp {timestamp:?}: {error}"));
    }

    for timestamp in [
        "2026-09-22T10:00:00z",
        "2016-12-31T23:59:60Z",
        "2026-09-22T10:00:00+0:1",
        "2026-09-22X10:00+0:1",
        "2026-09-22T10:00:00+00::00",
    ] {
        let scratch = Scratch::new();
        let events = scratch.events();
        one_event_log(&events);
        rewrite_first_event_timestamp_with_python_hash(&events, timestamp);
        assert!(
            !python_replay(&events).status.success(),
            "Python accepted invalid fully-hashed event timestamp {timestamp:?}"
        );
        assert!(
            replay(&events).is_err(),
            "Rust accepted invalid fully-hashed event timestamp {timestamp:?}"
        );
    }
}

#[test]
fn unknown_event_kind_fails_closed_after_valid_envelope() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let first = one_event_log(&events);
    append_event(&events, 2, &first, "future-state-transition", json!({}));
    assert!(replay(&events)
        .expect_err("unknown event kind must fail")
        .to_string()
        .contains("unknown event kind"));
}

#[test]
fn known_non_state_event_kinds_are_tolerated_after_envelope_validation() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let mut previous = one_event_log(&events);
    let kinds = [
        "handoff-read",
        "handoff-removed",
        "handoff-write-completed",
        "handoff-write-intended",
        "handoff-written",
        "legacy-validate-checkout-removed",
        "owner-released",
        "ownerless-agent-cache-relocated",
        "ownerless-agent-worktree-removed",
        "ownerless-validate-path-removed",
        "partial-updates-recovered",
    ];
    for (index, kind) in kinds.iter().enumerate() {
        previous = append_event(
            &events,
            u64::try_from(index).expect("test index") + 2,
            &previous,
            kind,
            json!({}),
        );
    }
    let summary = replay(&events).expect("known non-state kinds replay");
    assert_eq!(summary.replay_count, 12);
    assert_eq!(summary.tip_sha256, previous);
    assert_eq!(summary.active_revision, 3);
    assert_eq!(summary.archive_revision, 7);
}

#[test]
fn index_rebuild_materializes_and_reads_summary() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let tip = one_event_log(&events);
    let index = scratch.0.join("observer.sqlite");

    let rebuilt = rebuild_index(&events, &index).expect("rebuild index");
    assert_eq!(rebuilt.replay_count, 1);
    assert_eq!(rebuilt.tip_sha256, tip);
    assert_eq!(rebuilt.active_count, 1);
    assert_eq!(read_index(&index).expect("read index"), rebuilt);
}

#[test]
fn event_replaced_by_fifo_after_enumeration_fails_without_blocking() {
    let scratch = Scratch::new();
    let events = scratch.events();
    one_event_log(&events);
    let event = events.join("00000000000000000001.json");

    let error = replay_with_post_count_hook(&events, || {
        fs::remove_file(&event).expect("remove enumerated event");
        let status = Command::new("mkfifo")
            .arg("--")
            .arg(&event)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo failed with {status}");
    })
    .expect_err("post-enumeration FIFO substitution must fail closed");
    assert!(
        error.to_string().contains("not a regular file"),
        "unexpected FIFO error: {error}"
    );
}

#[test]
fn policy_input_replaced_by_fifo_after_inspection_fails_without_blocking() {
    let scratch = Scratch::new();
    for name in ["policy.json", "evidence.json"] {
        let input = scratch.0.join(name);
        fs::write(&input, b"{}\n").expect("write initial policy input");
        let result =
            load_typed_json_with_post_inspect_hook::<Value>(&input, "test policy input", || {
                fs::remove_file(&input).expect("remove inspected policy input");
                let status = Command::new("mkfifo")
                    .arg("--")
                    .arg(&input)
                    .status()
                    .expect("run mkfifo");
                assert!(status.success(), "mkfifo failed with {status}");
            });
        let error = match result {
            Ok(_) => panic!("post-inspection FIFO substitution must fail closed"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("changed while it was opened"),
            "unexpected {name} FIFO error: {error}"
        );
    }
}

#[test]
fn failed_first_rebuild_leaves_no_terminal_index() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let index = scratch.0.join("observer.sqlite");

    let error = rebuild_index(&events, &index).expect_err("empty replay must fail");
    assert!(error.to_string().contains("event log") && error.to_string().contains("empty"));
    assert!(!index.exists(), "failed first rebuild published an index");
    let leftovers = fs::read_dir(&scratch.0)
        .expect("list scratch directory")
        .map(|entry| entry.expect("read scratch entry").file_name())
        .filter(|name| name.to_string_lossy().contains("wrkslotsd-private"))
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "private index leftovers: {leftovers:?}"
    );
}

#[test]
fn failed_late_first_rebuild_leaves_no_partial_terminal_index() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let first_tip = one_event_log(&events);
    append_event(&events, 2, &first_tip, "operation-completed", json!({}));
    let second = events.join("00000000000000000002.json");
    let mut damaged: Value =
        serde_json::from_slice(&fs::read(&second).expect("read event")).expect("parse event");
    damaged["sha256"] = Value::String("f".repeat(64));
    fs::write(
        &second,
        serde_json::to_vec_pretty(&damaged).expect("encode damaged event"),
    )
    .expect("write damaged event");
    let index = scratch.0.join("observer.sqlite");

    let error = rebuild_index(&events, &index).expect_err("late first replay failure must fail");
    assert!(error.to_string().contains("digest does not match"));
    assert!(
        !index.exists(),
        "failed late first rebuild published a partial index"
    );
    let leftovers = fs::read_dir(&scratch.0)
        .expect("list scratch directory")
        .map(|entry| entry.expect("read scratch entry").file_name())
        .filter(|name| name.to_string_lossy().contains("wrkslotsd-private"))
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "private index leftovers: {leftovers:?}"
    );
}

#[test]
fn initial_index_is_absent_until_complete_database_is_publishable() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let tip = one_event_log(&events);
    let index = scratch.0.join("observer.sqlite");
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let worker_events = events.clone();
    let worker_index = index.clone();
    let worker_ready = Arc::clone(&ready);
    let worker_release = Arc::clone(&release);
    let worker = thread::spawn(move || {
        rebuild_index_with_prepublication_hook(&worker_events, &worker_index, || {
            worker_ready.wait();
            worker_release.wait();
        })
    });

    ready.wait();
    assert!(
        !index.exists(),
        "terminal path became visible before publication"
    );
    assert!(read_index(&index)
        .expect_err("unpublished index must be absent")
        .to_string()
        .contains("does not exist"));
    release.wait();
    let summary = worker
        .join()
        .expect("initial rebuild worker did not panic")
        .expect("publish completed index");
    assert_eq!(summary.tip_sha256, tip);
    assert_eq!(read_index(&index).expect("read published index"), summary);
}

#[test]
fn initial_index_is_complete_at_its_first_visible_instant() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let tip = one_event_log(&events);
    let index = scratch.0.join("observer.sqlite");
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let worker_events = events.clone();
    let worker_index = index.clone();
    let worker_ready = Arc::clone(&ready);
    let worker_release = Arc::clone(&release);
    let worker = thread::spawn(move || {
        rebuild_index_with_postpublication_hook(&worker_events, &worker_index, |private_path| {
            assert!(
                !private_path.exists(),
                "atomic rename retained its private source name"
            );
            worker_ready.wait();
            worker_release.wait();
        })
    });

    ready.wait();
    let visible = read_index(&index).expect("newly visible index must already be complete");
    assert_eq!(visible.tip_sha256, tip);
    let private_siblings = fs::read_dir(&scratch.0)
        .expect("list scratch directory")
        .map(|entry| entry.expect("read scratch entry").file_name())
        .filter(|name| name.to_string_lossy().contains("wrkslotsd-private"))
        .collect::<Vec<_>>();
    assert!(
        private_siblings.is_empty(),
        "published index retained private siblings: {private_siblings:?}"
    );
    release.wait();
    let summary = worker
        .join()
        .expect("initial rebuild worker did not panic")
        .expect("finish initial publication");
    assert_eq!(summary, visible);
}

#[test]
fn index_preserves_python_accepted_u64_revisions_losslessly() {
    let scratch = Scratch::new();
    let events = scratch.events();
    append_event(
        &events,
        1,
        &"0".repeat(64),
        "state-imported",
        imported_state(vec![active_record("slot-a", 1)], u64::MAX, vec![], u64::MAX),
    );
    let python = python_replay(&events);
    assert!(
        python.status.success(),
        "Python rejected maximum u64 revisions: {}",
        String::from_utf8_lossy(&python.stderr)
    );
    let replayed = replay(&events).expect("Rust replays maximum u64 revisions");
    assert_eq!(replayed.active_revision, u64::MAX);
    assert_eq!(replayed.archive_revision, u64::MAX);

    let index = scratch.0.join("observer.sqlite");
    let rebuilt = rebuild_index(&events, &index).expect("index maximum u64 revisions");
    assert_eq!(rebuilt, replayed);
    assert_eq!(
        read_index(&index).expect("read maximum revisions"),
        replayed
    );

    let connection = Connection::open(&index).expect("open maximum-revision index");
    let (active, active_type, archive, archive_type): (String, String, String, String) = connection
        .query_row(
            "SELECT active_revision, typeof(active_revision), archive_revision, typeof(archive_revision) FROM observer_metadata",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read lossless revision cells");
    assert_eq!(active, u64::MAX.to_string());
    assert_eq!(archive, u64::MAX.to_string());
    assert_eq!(
        (active_type.as_str(), archive_type.as_str()),
        ("text", "text")
    );
}

#[test]
fn failed_transaction_does_not_partially_replace_previous_index() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let first_tip = one_event_log(&events);
    let index = scratch.0.join("observer.sqlite");
    let original = rebuild_index(&events, &index).expect("initial rebuild");

    append_event(&events, 2, &first_tip, "operation-completed", json!({}));
    let second = events.join("00000000000000000002.json");
    let mut damaged: Value =
        serde_json::from_slice(&fs::read(&second).expect("read event")).expect("parse event");
    damaged["sha256"] = Value::String("f".repeat(64));
    fs::write(
        &second,
        serde_json::to_vec_pretty(&damaged).expect("encode damaged event"),
    )
    .expect("write damaged event");

    let error = rebuild_index(&events, &index).expect_err("late replay failure must roll back");
    assert!(error.to_string().contains("digest does not match"));
    assert_eq!(
        read_index(&index).expect("old index remains readable"),
        original
    );

    let connection = Connection::open(&index).expect("reopen derived index");
    let event_count: u64 = connection
        .query_row("SELECT COUNT(*) FROM event_log", [], |row| row.get(0))
        .expect("count retained event rows");
    assert_eq!(event_count, 1);
    let active_count: u64 = connection
        .query_row("SELECT COUNT(*) FROM active_records", [], |row| row.get(0))
        .expect("count retained active rows");
    assert_eq!(active_count, 1);
}

#[test]
fn complete_nested_schema_rejects_malformed_records_and_naive_time() {
    let missing_field = Scratch::new();
    let events = missing_field.events();
    let mut record = active_record("slot-a", 1);
    record["checkouts"][0]
        .as_object_mut()
        .expect("checkout object")
        .remove("head");
    import_log(&events, vec![record], vec![]);
    assert!(replay(&events)
        .expect_err("missing nested field must fail")
        .to_string()
        .contains("invalid fields"));

    let naive_time = Scratch::new();
    let events = naive_time.events();
    let mut record = active_record("slot-a", 1);
    record["heartbeat_at"] = Value::String("2026-09-22T09:30:00".to_owned());
    import_log(&events, vec![record], vec![]);
    assert!(replay(&events)
        .expect_err("timezone-naive record must fail")
        .to_string()
        .contains("timezone-naive"));

    let bad_archive = Scratch::new();
    let events = bad_archive.events();
    let mut record = archive_record("slot-old", 1);
    record["physical_storage"] = Value::String("present".to_owned());
    import_log(&events, vec![], vec![record]);
    assert!(replay(&events)
        .expect_err("invalid archive storage must fail")
        .to_string()
        .contains("removed physical storage"));
}

#[test]
fn active_agent_uniqueness_honors_import_exception_and_removal() {
    let duplicate = Scratch::new();
    let events = duplicate.events();
    import_log(
        &events,
        vec![
            active_record_for("slot-a", "worker", 1, "agent"),
            active_record_for("slot-b", "worker", 1, "agent"),
        ],
        vec![],
    );
    assert!(replay(&events)
        .expect_err("ordinary duplicate agent must fail")
        .to_string()
        .contains("owns both active slots"));
    assert!(!python_replay(&events).status.success());

    let imported = Scratch::new();
    let events = imported.events();
    import_log(
        &events,
        vec![
            imported_active_record("slot-a", "worker", 1),
            imported_active_record("slot-b", "worker", 1),
        ],
        vec![],
    );
    assert_eq!(
        replay(&events)
            .expect("historical imports do not claim live agent uniqueness")
            .active_count,
        2
    );
    let python = python_replay(&events);
    assert!(
        python.status.success(),
        "Python rejected duplicate historical agents: {}",
        String::from_utf8_lossy(&python.stderr)
    );

    let removal = Scratch::new();
    let events = removal.events();
    let record = active_record_for("slot-a", "worker", 1, "agent");
    let first = import_log(&events, vec![record.clone()], vec![]);
    let second = append_event(
        &events,
        2,
        &first,
        "active-state-recorded",
        json!({
            "action": "released",
            "slot": "slot-a",
            "previous_revision": 0,
            "revision": 1,
            "previous_record_sha256": canonical_sha256(&record).expect("record digest"),
            "record": null,
            "evidence": {},
        }),
    );
    append_event(
        &events,
        3,
        &second,
        "active-state-recorded",
        json!({
            "action": "allocated",
            "slot": "slot-b",
            "previous_revision": 1,
            "revision": 2,
            "previous_record_sha256": null,
            "record": active_record_for("slot-b", "worker", 1, "agent"),
            "evidence": {},
        }),
    );
    let summary = replay(&events).expect("removal releases live agent identity");
    assert_eq!((summary.active_revision, summary.active_count), (2, 1));

    let conflicting_update = Scratch::new();
    let events = conflicting_update.events();
    let slot_a = active_record_for("slot-a", "worker", 1, "agent");
    let slot_b = active_record_for("slot-b", "other", 1, "agent");
    let first = import_log(&events, vec![slot_a, slot_b.clone()], vec![]);
    append_event(
        &events,
        2,
        &first,
        "active-state-recorded",
        json!({
            "action": "heartbeat",
            "slot": "slot-b",
            "previous_revision": 0,
            "revision": 1,
            "previous_record_sha256": canonical_sha256(&slot_b).expect("record digest"),
            "record": active_record_for("slot-b", "worker", 1, "agent"),
            "evidence": {},
        }),
    );
    assert!(replay(&events)
        .expect_err("agent-conflicting update must fail")
        .to_string()
        .contains("already owns another active slot"));
}

fn append_recovery_pair(events: &Path, active: &Value, slot_type: &str, action: &str) -> String {
    let first = import_log(events, vec![active.clone()], vec![]);
    let archived = archive_record_for("slot-a", 1, slot_type);
    let archive_id = archived["archive_id"].as_str().expect("archive id");
    let source_record_sha256 = canonical_sha256(active).expect("source record digest");
    let evidence = if action == "absent-validation-row-recovered" {
        json!({
            "archive_id": archive_id,
            "source_record_sha256": source_record_sha256,
            "validation_outcome": "unknown",
        })
    } else {
        json!({
            "archive_id": archive_id,
            "source_record_sha256": source_record_sha256,
            "physical_storage": "externally-absent",
        })
    };
    let second = append_event(
        events,
        2,
        &first,
        "archive-state-recorded",
        json!({
            "action": action,
            "slot": "slot-a",
            "previous_revision": 0,
            "revision": 1,
            "record": archived,
            "evidence": evidence.clone(),
        }),
    );
    append_event(
        events,
        3,
        &second,
        "active-state-recorded",
        json!({
            "action": action,
            "slot": "slot-a",
            "previous_revision": 0,
            "revision": 1,
            "previous_record_sha256": source_record_sha256,
            "record": null,
            "evidence": evidence,
        }),
    )
}

#[test]
fn absent_recovery_evidence_is_exact_for_both_slot_types() {
    for (slot_type, action) in [
        ("validate", "absent-validation-row-recovered"),
        ("agent", "absent-agent-row-recovered"),
    ] {
        let scratch = Scratch::new();
        let events = scratch.events();
        let active = active_record_for("slot-a", "worker", 1, slot_type);
        append_recovery_pair(&events, &active, slot_type, action);
        let summary = replay(&events).expect("typed absent-row recovery");
        assert_eq!((summary.active_count, summary.archive_count), (0, 1));
        let python = python_replay(&events);
        assert!(
            python.status.success(),
            "Python rejected typed recovery: {}",
            String::from_utf8_lossy(&python.stderr)
        );
    }

    let wrong_source = Scratch::new();
    let events = wrong_source.events();
    let active = active_record_for("slot-a", "worker", 1, "validate");
    append_recovery_pair(
        &events,
        &active,
        "validate",
        "absent-validation-row-recovered",
    );
    let path = events.join("00000000000000000003.json");
    let mut event: Value = serde_json::from_slice(&fs::read(&path).expect("read recovery event"))
        .expect("parse recovery event");
    event["payload"]["evidence"]["source_record_sha256"] = Value::String("f".repeat(64));
    let mut core = event.as_object().expect("event object").clone();
    core.remove("sha256");
    event["sha256"] =
        Value::String(canonical_sha256(&Value::Object(core)).expect("recompute envelope digest"));
    fs::write(
        &path,
        serde_json::to_vec_pretty(&event).expect("encode event"),
    )
    .expect("write event");
    assert!(replay(&events)
        .expect_err("wrong recovery source digest must fail")
        .to_string()
        .contains("source record"));
    assert!(!python_replay(&events).status.success());

    let wrong_type = Scratch::new();
    let events = wrong_type.events();
    let active = active_record_for("slot-a", "worker", 1, "validate");
    append_recovery_pair(&events, &active, "validate", "absent-agent-row-recovered");
    assert!(replay(&events)
        .expect_err("agent recovery for validation row must fail")
        .to_string()
        .contains("archive agent recovery evidence does not match its record"));
    assert!(!python_replay(&events).status.success());
}

#[test]
fn older_and_forked_replays_cannot_replace_a_newer_index() {
    let source = Scratch::new();
    let events = source.events();
    let first = one_event_log(&events);
    append_event(&events, 2, &first, "handoff-written", json!({}));
    let index = source.0.join("observer.sqlite");
    let indexed = rebuild_index(&events, &index).expect("index two events");

    let older = Scratch::new();
    let older_events = older.events();
    one_event_log(&older_events);
    assert!(rebuild_index(&older_events, &index)
        .expect_err("older replay must fail")
        .to_string()
        .contains("older"));
    assert_eq!(read_index(&index).expect("newer index retained"), indexed);

    let forked = Scratch::new();
    let forked_events = forked.events();
    let fork_first = one_event_log(&forked_events);
    append_event(
        &forked_events,
        2,
        &fork_first,
        "handoff-written",
        json!({"different": true}),
    );
    assert!(rebuild_index(&forked_events, &index)
        .expect_err("forked replay must fail")
        .to_string()
        .contains("does not extend"));
    assert_eq!(
        read_index(&index).expect("unforked index retained"),
        indexed
    );

    let second_tip = indexed.tip_sha256.clone();
    append_event(&events, 3, &second_tip, "handoff-written", json!({}));
    let extended = rebuild_index(&events, &index).expect("strict extension replaces index");
    assert_eq!(extended.replay_count, 3);
}

#[test]
fn incompatible_index_schema_is_not_overwritten() {
    let scratch = Scratch::new();
    let events = scratch.events();
    one_event_log(&events);
    let index = scratch.0.join("observer.sqlite");
    rebuild_index(&events, &index).expect("build index");
    let connection = Connection::open(&index).expect("open index");
    connection
        .execute("UPDATE observer_metadata SET schema_version = 99", [])
        .expect("install incompatible schema marker");
    drop(connection);
    assert!(rebuild_index(&events, &index)
        .expect_err("incompatible index must not be overwritten")
        .to_string()
        .contains("unsupported derived index schema 99"));
    let connection = Connection::open(&index).expect("reopen incompatible index");
    let schema: u64 = connection
        .query_row(
            "SELECT schema_version FROM observer_metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("read retained schema marker");
    assert_eq!(schema, 99);
}

#[test]
fn concurrent_rebuilders_cannot_publish_a_partial_index() {
    let scratch = Scratch::new();
    let events = scratch.events();
    one_event_log(&events);
    let index = scratch.0.join("observer.sqlite");
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let barrier = Arc::clone(&barrier);
        let events = events.clone();
        let index = index.clone();
        workers.push(thread::spawn(move || {
            rebuild_index_with_prepublication_hook(&events, &index, || {
                barrier.wait();
            })
        }));
    }
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("rebuild worker did not panic"))
        .collect::<Vec<_>>();
    assert!(
        results.iter().all(Result::is_ok),
        "concurrent rebuild failures: {results:?}"
    );
    let summary = read_index(&index).expect("concurrent rebuild left one complete index");
    assert_eq!((summary.replay_count, summary.active_count), (1, 1));
}

#[test]
fn index_symlinks_and_hardlinks_fail_closed() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let events = scratch.events();
    one_event_log(&events);
    let index = scratch.0.join("observer.sqlite");
    rebuild_index(&events, &index).expect("build source index");

    let symbolic = scratch.0.join("symbolic.sqlite");
    symlink(&index, &symbolic).expect("create index symlink");
    assert!(read_index(&symbolic)
        .expect_err("index symlink must fail")
        .to_string()
        .contains("not a real regular file"));

    let hard = scratch.0.join("hard.sqlite");
    fs::hard_link(&index, &hard).expect("create index hard link");
    assert!(read_index(&hard)
        .expect_err("index hard link must fail")
        .to_string()
        .contains("multiple hard links"));
    assert!(rebuild_index(&events, &index)
        .expect_err("linked index rebuild must fail")
        .to_string()
        .contains("multiple hard links"));
}

#[test]
fn index_inside_event_directory_is_rejected_without_touching_events() {
    let scratch = Scratch::new();
    let events = scratch.events();
    one_event_log(&events);
    let index = events.join("observer.sqlite");
    assert!(rebuild_index(&events, &index)
        .expect_err("index inside event authority must fail")
        .to_string()
        .contains("must not be placed"));
    assert!(!index.exists());
}

#[test]
fn event_size_limit_accepts_the_exact_boundary_and_rejects_one_byte_more() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let path = events.join("00000000000000000001.json");
    one_event_log(&events);
    let mut event: Value =
        serde_json::from_slice(&fs::read(&path).expect("read event")).expect("parse event");
    event["payload"]["active"]["slots"][0]["purpose"] = Value::String(String::new());
    let empty = serde_json::to_vec_pretty(&event).expect("encode empty-padding event");
    let limit = usize::try_from(MAX_EVENT_BYTES).expect("event limit fits usize");
    let padding = limit
        .checked_sub(empty.len())
        .expect("fixture envelope fits under event limit");
    event["payload"]["active"]["slots"][0]["purpose"] = Value::String("x".repeat(padding));
    let mut core = event.as_object().expect("event object").clone();
    core.remove("sha256");
    event["sha256"] =
        Value::String(canonical_sha256(&Value::Object(core)).expect("hash exact-limit event"));
    let encoded = serde_json::to_vec_pretty(&event).expect("encode exact-limit event");
    assert_eq!(encoded.len(), limit);
    fs::write(&path, encoded).expect("write exact-limit event");
    replay(&events).expect("exact event-size limit must be accepted");

    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open exact-limit event")
        .write_all(b" ")
        .expect("extend event beyond limit");
    assert!(replay(&events)
        .expect_err("oversized event must fail")
        .to_string()
        .contains("replay limit"));
}

#[test]
fn python_and_rust_agree_on_valid_records_and_adversarial_schema() {
    let valid = Scratch::new();
    let events = valid.events();
    import_log(
        &events,
        vec![active_record("slot-a", 1)],
        vec![archive_record("slot-old", 2)],
    );
    let python = python_replay(&events);
    assert!(
        python.status.success(),
        "Python rejected valid fixture: {}",
        String::from_utf8_lossy(&python.stderr)
    );
    let python: Value = serde_json::from_slice(&python.stdout).expect("parse Python result");
    let index = valid.0.join("differential.sqlite");
    let rust = rebuild_index(&events, &index).expect("Rust accepts valid fixture");
    assert_eq!(python["machine"], rust.machine);
    assert_eq!(python["replay_count"], rust.replay_count);
    assert_eq!(python["tip_sha256"], rust.tip_sha256);
    assert_eq!(python["active_revision"], rust.active_revision);
    assert_eq!(python["archive_revision"], rust.archive_revision);

    let connection = Connection::open(&index).expect("open differential index");
    let mut active_statement = connection
        .prepare("SELECT record_json FROM active_records ORDER BY slot")
        .expect("prepare active query");
    let active = active_statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query active records")
        .map(|row| {
            serde_json::from_str::<Value>(&row.expect("read active record"))
                .expect("parse active record")
        })
        .collect::<Vec<_>>();
    assert_eq!(python["active_records"], Value::Array(active));
    let mut archive_statement = connection
        .prepare("SELECT record_json FROM archive_records ORDER BY rowid")
        .expect("prepare archive query");
    let archive = archive_statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query archive records")
        .map(|row| {
            serde_json::from_str::<Value>(&row.expect("read archive record"))
                .expect("parse archive record")
        })
        .collect::<Vec<_>>();
    assert_eq!(python["archive_records"], Value::Array(archive));

    for damage in ["nested-checkout", "duplicate-agent", "naive-timestamp"] {
        let rejected = Scratch::new();
        let events = rejected.events();
        let mut first = active_record_for("slot-a", "worker", 1, "agent");
        let mut records = vec![first.clone()];
        match damage {
            "nested-checkout" => {
                first["checkouts"][0]
                    .as_object_mut()
                    .expect("checkout object")
                    .remove("head");
                records = vec![first];
            }
            "duplicate-agent" => {
                records.push(active_record_for("slot-b", "worker", 1, "agent"));
            }
            "naive-timestamp" => {
                first["created_at"] = Value::String("2026-09-22T09:00:00".to_owned());
                records = vec![first];
            }
            _ => unreachable!(),
        }
        import_log(&events, records, vec![]);
        assert!(replay(&events).is_err(), "Rust accepted {damage}");
        let python = python_replay(&events);
        assert!(!python.status.success(), "Python accepted {damage}");
    }
}

#[test]
fn python_authority_round_trips_task_scoped_active_records() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = scoped_active_record("slot-a", 1);
    import_log(&events, vec![record.clone()], vec![]);
    let python = python_replay(&events);
    assert!(
        python.status.success(),
        "Python rejected task scope identity: {}",
        String::from_utf8_lossy(&python.stderr)
    );
    let python: Value = serde_json::from_slice(&python.stdout).expect("parse Python replay");
    assert_eq!(
        python["active_records"][0]["task_scope"],
        record["task_scope"]
    );
    assert_eq!(
        replay(&events)
            .expect("Rust replays Python-compatible scope")
            .active_count,
        1
    );

    for (field, value) in [
        ("leader_pid", Value::String("9001".to_owned())),
        ("unit", Value::String("wrkslots-\u{00a0}.scope".to_owned())),
        (
            "cgroup_path",
            Value::String("/user.slice/prefix-wrkslots-slot-a-1.scope".to_owned()),
        ),
        ("boot_id", Value::String("not-a-boot-id".to_owned())),
    ] {
        let rejected = Scratch::new();
        let rejected_events = rejected.events();
        let mut malformed = scoped_active_record("slot-a", 1);
        malformed["task_scope"][field] = value;
        import_log(&rejected_events, vec![malformed], vec![]);
        assert!(
            replay(&rejected_events).is_err(),
            "Rust accepted malformed task-scope field {field}"
        );
        assert!(
            !python_replay(&rejected_events).status.success(),
            "Python accepted malformed task-scope field {field}"
        );
    }
    for control in [
        '\u{1c}',
        '\u{1d}',
        '\u{1e}',
        '\u{1f}',
        '\u{00ad}',
        '\u{061c}',
        '\u{200b}',
        '\u{200e}',
        '\u{202e}',
        '\u{2066}',
        '\u{2069}',
        '\u{feff}',
        '\u{1bca0}',
        '\u{e0001}',
    ] {
        let rejected = Scratch::new();
        let rejected_events = rejected.events();
        let mut malformed = scoped_active_record("slot-a", 1);
        let unit = format!("wrkslots-{control}.scope");
        malformed["task_scope"]["unit"] = Value::String(unit.clone());
        malformed["task_scope"]["cgroup_path"] = Value::String(format!("/user.slice/{unit}"));
        malformed["owner"]["cgroup_path"] = Value::String(format!("/user.slice/{unit}"));
        import_log(&rejected_events, vec![malformed], vec![]);
        assert!(
            replay(&rejected_events).is_err(),
            "Rust accepted task-scope unit containing U+{:04X}",
            control as u32
        );
        assert!(
            !python_replay(&rejected_events).status.success(),
            "Python accepted task-scope unit containing U+{:04X}",
            control as u32
        );
    }
    let visible_unicode = Scratch::new();
    let visible_events = visible_unicode.events();
    let mut visible = scoped_active_record("slot-a", 1);
    visible["task_scope"]["unit"] = Value::String("wrkslots-é.scope".to_owned());
    visible["task_scope"]["cgroup_path"] = Value::String("/user.slice/wrkslots-é.scope".to_owned());
    visible["owner"]["cgroup_path"] = Value::String("/user.slice/wrkslots-é.scope".to_owned());
    import_log(&visible_events, vec![visible], vec![]);
    replay(&visible_events).expect("Rust accepts visible non-format Unicode in scope unit");
    assert!(
        python_replay(&visible_events).status.success(),
        "Python rejects visible non-format Unicode in scope unit"
    );
    for (field, value) in [
        ("leader_pid", json!(2001)),
        ("leader_start_ticks", json!(2101)),
        ("cgroup_path", json!("/other.slice/wrkslots-slot-a-1.scope")),
        ("boot_id", json!("bbbbbbbb-cccc-dddd-eeee-ffffffffffff")),
    ] {
        let rejected = Scratch::new();
        let rejected_events = rejected.events();
        let mut mismatched = scoped_active_record("slot-a", 1);
        mismatched["task_scope"][field] = value;
        import_log(&rejected_events, vec![mismatched], vec![]);
        assert!(
            replay(&rejected_events).is_err(),
            "Rust accepted task-scope/owner mismatch in {field}"
        );
        assert!(
            !python_replay(&rejected_events).status.success(),
            "Python accepted task-scope/owner mismatch in {field}"
        );
    }
    let rejected = Scratch::new();
    let rejected_events = rejected.events();
    let mut ownerless = scoped_active_record("slot-a", 1);
    ownerless["owner"] = Value::Null;
    import_log(&rejected_events, vec![ownerless], vec![]);
    assert!(replay(&rejected_events).is_err());
    assert!(!python_replay(&rejected_events).status.success());
}

#[test]
fn python_normalized_active_records_drive_storage_and_following_hashes() {
    let scratch = Scratch::new();
    let events = scratch.events();

    let mut omitted_slot_type = active_record_for("slot-a", "worker-a", 1, "agent");
    omitted_slot_type
        .as_object_mut()
        .expect("active record object")
        .remove("slot_type");
    let mut normalized_slot_type = omitted_slot_type.clone();
    normalized_slot_type["slot_type"] = Value::String("agent".to_owned());

    let mut null_import = active_record_for("slot-b", "worker-b", 1, "agent");
    null_import["import_source"] = Value::Null;
    let mut normalized_import = null_import.clone();
    normalized_import
        .as_object_mut()
        .expect("active record object")
        .remove("import_source");

    let first = import_log(
        &events,
        vec![omitted_slot_type.clone(), null_import.clone()],
        vec![],
    );
    let mut next_slot_type = omitted_slot_type;
    next_slot_type["heartbeat_at"] = Value::String("2026-09-22T09:31:00+00:00".to_owned());
    let second = append_event(
        &events,
        2,
        &first,
        "active-state-recorded",
        json!({
            "action": "heartbeat",
            "slot": "slot-a",
            "previous_revision": 0,
            "revision": 1,
            "previous_record_sha256": canonical_sha256(&normalized_slot_type)
                .expect("normalized slot-type digest"),
            "record": next_slot_type,
            "evidence": {},
        }),
    );
    let mut next_import = null_import;
    next_import["heartbeat_at"] = Value::String("2026-09-22T09:31:00+00:00".to_owned());
    append_event(
        &events,
        3,
        &second,
        "active-state-recorded",
        json!({
            "action": "heartbeat",
            "slot": "slot-b",
            "previous_revision": 1,
            "revision": 2,
            "previous_record_sha256": canonical_sha256(&normalized_import)
                .expect("normalized import-source digest"),
            "record": next_import,
            "evidence": {},
        }),
    );

    let python = python_replay(&events);
    assert!(
        python.status.success(),
        "Python rejected normalization fixture: {}",
        String::from_utf8_lossy(&python.stderr)
    );
    let python: Value = serde_json::from_slice(&python.stdout).expect("parse Python result");
    let summary = replay(&events).expect("Rust accepts Python-normalized transitions");
    assert_eq!((summary.active_revision, summary.active_count), (2, 2));
    let index = scratch.0.join("normalized.sqlite");
    rebuild_index(&events, &index).expect("index normalized active state");
    let connection = Connection::open(&index).expect("open normalized index");
    let stored = connection
        .prepare("SELECT record_json FROM active_records ORDER BY slot")
        .expect("prepare normalized record query")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query normalized records")
        .map(|row| serde_json::from_str::<Value>(&row.expect("read record")).expect("parse record"))
        .collect::<Vec<_>>();
    assert_eq!(python["active_records"], Value::Array(stored.clone()));
    assert!(stored.iter().all(|record| record["slot_type"] == "agent"));
    assert!(stored
        .iter()
        .all(|record| record.get("import_source").is_none()));

    let rejected = Scratch::new();
    let rejected_events = rejected.events();
    let mut raw = active_record_for("slot-a", "worker-a", 1, "agent");
    raw.as_object_mut()
        .expect("active record object")
        .remove("slot_type");
    let first = import_log(&rejected_events, vec![raw.clone()], vec![]);
    append_event(
        &rejected_events,
        2,
        &first,
        "active-state-recorded",
        json!({
            "action": "heartbeat",
            "slot": "slot-a",
            "previous_revision": 0,
            "revision": 1,
            "previous_record_sha256": canonical_sha256(&raw).expect("raw record digest"),
            "record": raw,
            "evidence": {},
        }),
    );
    assert!(replay(&rejected_events).is_err());
    assert!(!python_replay(&rejected_events).status.success());
}

fn scoped_active_record(slot: &str, generation: u64) -> Value {
    let mut record = active_record(slot, generation);
    let unit = format!("wrkslots-{slot}-{generation}.scope");
    let cgroup_path = format!("/user.slice/user-1000.slice/{unit}");
    record["owner"]["boot_id"] = Value::String("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned());
    record["owner"]["cgroup_path"] = Value::String(cgroup_path.clone());
    record["task_scope"] = json!({
        "unit": unit,
        "invocation_id": "a".repeat(32),
        "cgroup_path": cgroup_path,
        "boot_id": record["owner"]["boot_id"].clone(),
        "leader_pid": record["owner"]["pid"].clone(),
        "leader_start_ticks": record["owner"]["start_ticks"].clone(),
        "verification": "systemd-runtime-invocation-symlink-v1",
    });
    record
}

fn write_policy_inputs(
    scratch: &Scratch,
    records: &[Value],
    observed_at: &str,
    mutate: impl Fn(&mut Value),
) -> (PathBuf, PathBuf) {
    let config = scratch.0.join("policy.json");
    let evidence = scratch.0.join("evidence.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!({
            "schema": 1,
            "machine": "node-a",
            "minimum_stale_seconds": 0,
            "max_plan_slots": 250,
            "evidence_max_age_seconds": 3155760000_u64,
            "maximum_census_seconds": 300,
            "maximum_future_skew_seconds": 60,
        }))
        .expect("encode policy config"),
    )
    .expect("write policy config");
    let slots = records
        .iter()
        .map(|record| {
            let slot = record["slot"].as_str().expect("record slot");
            let generation = record["generation"].as_u64().expect("record generation");
            let scope = if record["task_scope"].is_object() {
                json!({
                    "recorded": record["task_scope"].clone(),
                    "state": "dead",
                    "leader_state": "absent",
                })
            } else {
                Value::Null
            };
            json!({
                "slot": slot,
                "generation": generation,
                "active_record_sha256": canonical_sha256(record).expect("active digest"),
                "scope": scope,
                "checkouts": [{
                    "name": "source",
                    "path": record["checkouts"][0]["path"].clone(),
                    "device": 10 + generation,
                    "inode": 20 + generation,
                    "mount_id": 30 + generation,
                    "directory": true,
                    "symlink_free": true,
                    "mount_stable": true,
                }],
                "journal": "absent",
                "process_use": "unused",
                "reclaimable_bytes": 4096 + generation,
            })
        })
        .collect::<Vec<_>>();
    let mut value = json!({
        "schema": 1,
        "machine": "node-a",
        "event_tip_sha256": latest_event_tip(scratch),
        "census_started_at": observed_at,
        "observed_at": observed_at,
        "boot_id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        "slots": slots,
    });
    mutate(&mut value);
    fs::write(
        &evidence,
        serde_json::to_vec(&value).expect("encode policy evidence"),
    )
    .expect("write policy evidence");
    (config, evidence)
}

fn latest_event_tip(scratch: &Scratch) -> String {
    let events = scratch.0.join("project/worktrees/EVENTS.node-a");
    let latest = fs::read_dir(events)
        .expect("list event log")
        .map(|entry| entry.expect("read event entry").path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .max()
        .expect("event log is not empty");
    let value: Value =
        serde_json::from_slice(&fs::read(latest).expect("read latest event")).expect("parse event");
    value["sha256"].as_str().expect("event digest").to_owned()
}

fn rewrite_json(path: &Path, mutate: impl FnOnce(&mut Value)) {
    let mut value: Value =
        serde_json::from_slice(&fs::read(path).expect("read JSON fixture")).expect("parse fixture");
    mutate(&mut value);
    fs::write(
        path,
        serde_json::to_vec(&value).expect("encode JSON fixture"),
    )
    .expect("rewrite JSON fixture");
}

fn evaluate_policy_at(
    events: &Path,
    config: &Path,
    evidence: &Path,
    evaluated_at: &str,
) -> Result<Vec<crate::policy::PolicyDecision>, crate::ObserverError> {
    let config = ShadowConfig::load(config)?;
    let evidence = EvidenceBundle::load(evidence)?;
    let replayed = replay_stream(events, None, |_| Ok(()))?;
    policy::evaluate(
        &replayed,
        &config.value,
        &config.sha256,
        &evidence.value,
        &evidence.sha256,
        evaluated_at,
    )
}

fn policy_decision(
    scratch: &Scratch,
    record: Value,
    observed_at: &str,
    mutate: impl Fn(&mut Value),
) -> crate::policy::PolicyDecision {
    let events = scratch.events();
    import_log(&events, vec![record.clone()], vec![]);
    let (config, evidence) = write_policy_inputs(scratch, &[record], observed_at, mutate);
    let index = scratch.0.join("policy.sqlite");
    rebuild_policy_index(&events, &index, &config, &evidence).expect("build policy index");
    read_decision(&index, "slot-a").expect("read policy decision")
}

fn direct_policy_decision(
    census_started_at: &str,
    observed_at: &str,
    evaluated_at: &str,
    minimum_stale_seconds: u64,
    evidence_max_age_seconds: u64,
    maximum_census_seconds: u64,
    maximum_future_skew_seconds: u64,
) -> Result<crate::policy::PolicyDecision, crate::ObserverError> {
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = scoped_active_record("slot-a", 1);
    import_log(&events, vec![record.clone()], vec![]);
    let (config, evidence) = write_policy_inputs(&scratch, &[record], observed_at, |_| {});
    rewrite_json(&config, |value| {
        value["minimum_stale_seconds"] = json!(minimum_stale_seconds);
        value["evidence_max_age_seconds"] = json!(evidence_max_age_seconds);
        value["maximum_census_seconds"] = json!(maximum_census_seconds);
        value["maximum_future_skew_seconds"] = json!(maximum_future_skew_seconds);
    });
    rewrite_json(&evidence, |value| {
        value["census_started_at"] = Value::String(census_started_at.to_owned());
    });
    evaluate_policy_at(&events, &config, &evidence, evaluated_at)
        .map(|mut decisions| decisions.remove(0))
}

fn assert_rollout_gated(decision: &crate::policy::PolicyDecision) {
    assert_eq!(decision.verdict, Verdict::Unknown);
    for reason in [
        "TASK_SCOPE_RUNTIME_UNVERIFIED",
        "TASKGRAPH_CLAIM_UNVERIFIED",
    ] {
        assert!(
            decision.reason_codes.iter().any(|found| found == reason),
            "missing rollout gate {reason}: {:?}",
            decision.reason_codes
        );
    }
}

#[test]
fn active_state_cannot_remove_or_replace_a_held_generation() {
    let held_log = |next: Value| {
        let scratch = Scratch::new();
        let events = scratch.events();
        let record = scoped_active_record("slot-a", 1);
        let first = import_log(&events, vec![record.clone()], vec![]);
        let held = append_event(
            &events,
            2,
            &first,
            "slot-held",
            json!({"slot": "slot-a", "generation": 1, "reason": "owner pause"}),
        );
        append_event(
            &events,
            3,
            &held,
            "active-state-recorded",
            json!({
                "action": "heartbeat",
                "slot": "slot-a",
                "previous_revision": 0,
                "revision": 1,
                "previous_record_sha256": canonical_sha256(&record).expect("record digest"),
                "record": next,
                "evidence": {},
            }),
        );
        replay(&events)
    };

    // Control: the held generation itself may still be rewritten.
    let mut heartbeat = scoped_active_record("slot-a", 1);
    heartbeat["heartbeat_at"] = Value::String("2026-09-22T09:31:00+00:00".to_owned());
    let summary = held_log(heartbeat).expect("heartbeat on the held generation replays");
    assert_eq!((summary.active_revision, summary.active_count), (1, 1));

    for (case, next) in [
        ("removal", Value::Null),
        ("new generation", scoped_active_record("slot-a", 2)),
    ] {
        let error = held_log(next)
            .err()
            .unwrap_or_else(|| panic!("{case} under a hold must be refused"))
            .to_string();
        assert!(
            error.contains("active-state-recorded changes held generation for slot-a"),
            "{case}: {error}"
        );
    }
}

#[test]
fn hold_events_are_typed_generation_bound_and_released() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = scoped_active_record("slot-a", 1);
    let first = import_log(&events, vec![record.clone()], vec![]);
    let held = append_event(
        &events,
        2,
        &first,
        "slot-held",
        json!({"slot": "slot-a", "generation": 1, "reason": "owner pause"}),
    );
    let (config, evidence) = write_policy_inputs(
        &scratch,
        std::slice::from_ref(&record),
        "2026-09-22T09:40:01+00:00",
        |_| {},
    );

    let plain_index = scratch.0.join("held-without-policy.sqlite");
    rebuild_index(&events, &plain_index).expect("index held state without policy inputs");
    let plain_decision =
        read_decision(&plain_index, "slot-a").expect("read held no-input decision");
    assert_eq!(plain_decision.verdict, Verdict::Blocked);
    assert_eq!(
        plain_decision.reason_codes,
        ["POLICY_INPUTS_MISSING", "ACTIVE_HOLD"]
    );
    let plain_plan = read_pressure_plan(&plain_index, 1, None).expect("plan held no-input state");
    assert!(plain_plan.eligible.is_empty());
    assert!(plain_plan.deferred_eligible.is_empty());
    assert!(plain_plan.unknown.is_empty());
    assert_eq!(plain_plan.blocked, [plain_decision]);
    assert!(!plain_plan.target_met);

    let schema_one_index = scratch.0.join("held-schema-one.sqlite");
    write_schema_one_index(&events, &schema_one_index);
    let schema_one_decision =
        read_decision(&schema_one_index, "slot-a").expect("read held schema-one decision");
    assert_eq!(schema_one_decision.verdict, Verdict::Blocked);
    assert_eq!(
        schema_one_decision.reason_codes,
        ["LEGACY_INDEX_SCHEMA", "ACTIVE_HOLD"]
    );
    let schema_one_plan =
        read_pressure_plan(&schema_one_index, 1, None).expect("plan held schema-one state");
    assert!(schema_one_plan.eligible.is_empty());
    assert!(schema_one_plan.deferred_eligible.is_empty());
    assert!(schema_one_plan.unknown.is_empty());
    assert_eq!(schema_one_plan.blocked, [schema_one_decision]);
    assert!(!schema_one_plan.target_met);

    let index = scratch.0.join("held.sqlite");
    rebuild_policy_index(&events, &index, &config, &evidence).expect("index held state");
    let decision = read_decision(&index, "slot-a").expect("read held decision");
    assert_eq!(decision.verdict, Verdict::Blocked);
    assert!(decision
        .reason_codes
        .iter()
        .any(|reason| reason == "ACTIVE_HOLD"));

    append_event(
        &events,
        3,
        &held,
        "slot-hold-released",
        json!({"slot": "slot-a", "generation": 1}),
    );
    let (config, evidence) = write_policy_inputs(
        &scratch,
        std::slice::from_ref(&record),
        "2026-09-22T09:40:02+00:00",
        |_| {},
    );
    rebuild_policy_index(&events, &index, &config, &evidence).expect("index released state");
    let released = read_decision(&index, "slot-a").expect("read released decision");
    assert_rollout_gated(&released);
    assert!(!released
        .reason_codes
        .iter()
        .any(|reason| reason == "ACTIVE_HOLD"));

    let malformed = Scratch::new();
    let malformed_events = malformed.events();
    let first = import_log(
        &malformed_events,
        vec![scoped_active_record("slot-a", 1)],
        vec![],
    );
    append_event(
        &malformed_events,
        2,
        &first,
        "slot-held",
        json!({"slot": "slot-a", "generation": 2, "reason": "wrong generation"}),
    );
    assert!(replay(&malformed_events)
        .expect_err("mismatched hold generation must fail")
        .to_string()
        .contains("does not match active generation"));

    let malformed_import = Scratch::new();
    let malformed_import_events = malformed_import.events();
    let mut imported = imported_state(vec![scoped_active_record("slot-a", 1)], 0, vec![], 0);
    imported["holds"] = json!([{
        "schema": 1,
        "machine": "node-a",
        "slot": "slot-a",
        "held_at": "2026-09-22T09:00:00+00:00",
        "reason": "",
    }]);
    append_event(
        &malformed_import_events,
        1,
        &"0".repeat(64),
        "state-imported",
        imported,
    );
    assert!(replay(&malformed_import_events)
        .expect_err("empty imported hold reason must fail")
        .to_string()
        .contains("empty reason"));

    let missing_hold = Scratch::new();
    let missing_hold_events = missing_hold.events();
    let first = import_log(
        &missing_hold_events,
        vec![scoped_active_record("slot-a", 1)],
        vec![],
    );
    append_event(
        &missing_hold_events,
        2,
        &first,
        "slot-hold-released",
        json!({"slot": "slot-a", "generation": 1}),
    );
    assert!(replay(&missing_hold_events)
        .expect_err("release without hold must fail")
        .to_string()
        .contains("has no hold"));
}

#[test]
fn lifecycle_attempts_clear_only_after_matching_archive_and_active_removal() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = scoped_active_record("slot-a", 1);
    let mut tip = import_log(&events, vec![record.clone()], vec![]);
    tip = append_event(
        &events,
        2,
        &tip,
        "retirement-attempted",
        json!({
            "slot": "slot-a",
            "generation": 1,
            "sha256": "a".repeat(64),
            "handoff_read_sequence": 1,
            "reason": "bounded queue attempt",
        }),
    );
    tip = append_event(
        &events,
        3,
        &tip,
        "reclaim-started",
        json!({
            "slot": "slot-a",
            "generation": 1,
            "actor": identity(10),
            "runner": identity(11),
            "handoff_writer": null,
            "coordinator_authorized": true,
            "owner_state": "dead",
            "registered_liveness": "dead",
            "heartbeat_age_seconds": 601,
            "heartbeat_ttl_seconds": 600,
            "validate_complete": false,
            "live_validate_owner": false,
            "salvage_archive_root": null,
        }),
    );
    let journal = json!({
        "schema": 2,
        "kind": "finish",
        "machine": "node-a",
        "slot": "slot-a",
        "phase": "prepared",
        "record": record.clone(),
    });
    tip = append_event(
        &events,
        4,
        &tip,
        "operation-progress-recorded",
        json!({
            "slot": "slot-a",
            "operation": "finish",
            "journal_path": "FINISH.6.node-a.6.slot-a.journal",
            "journal": journal,
        }),
    );
    tip = append_event(
        &events,
        5,
        &tip,
        "recovery-started",
        json!({
            "slot": "slot-a",
            "operation": "finish",
            "actor": identity(12),
            "runner": identity(13),
            "handoff_writer": null,
            "coordinator_authorized": true,
        }),
    );

    let replayed = replay_stream(&events, None, |_| Ok(())).expect("replay open lifecycle");
    assert_eq!(replayed.pending_operations.len(), 4);
    let (config, evidence) = write_policy_inputs(
        &scratch,
        &[replayed.active_records["slot-a"].clone()],
        "2026-09-22T09:40:01+00:00",
        |_| {},
    );
    let decisions = evaluate_policy_at(&events, &config, &evidence, "2026-09-22T09:40:01+00:00")
        .expect("evaluate interrupted lifecycle");
    let decision = &decisions[0];
    assert_eq!(decision.verdict, Verdict::Blocked);
    for reason in [
        "RETIREMENT_PENDING",
        "RECLAIM_PENDING",
        "OPERATION_JOURNAL_PENDING",
        "RECOVERY_PENDING",
        "JOURNAL_EVENT_EVIDENCE_CONFLICT",
    ] {
        assert!(decision.reason_codes.iter().any(|found| found == reason));
    }

    let completed = append_event(
        &events,
        6,
        &tip,
        "operation-completed",
        json!({
            "slot": "slot-a",
            "operation": "finish",
            "journal_path": "FINISH.6.node-a.6.slot-a.journal",
        }),
    );
    let duplicate = append_event(
        &events,
        7,
        &completed,
        "operation-completed",
        json!({
            "slot": "slot-a",
            "operation": "finish",
            "journal_path": "FINISH.6.node-a.6.slot-a.journal",
        }),
    );
    let after_completion = replay_stream(&events, None, |_| Ok(()))
        .expect("matching and duplicate completions close only the journal");
    assert_eq!(after_completion.pending_operations.len(), 3);
    assert!(after_completion
        .pending_operations
        .iter()
        .all(|pending| pending.kind != PendingOperationKind::Journal));

    let archived_record = archive_record("slot-a", 1);
    let archived = append_event(
        &events,
        8,
        &duplicate,
        "archive-state-recorded",
        json!({
            "action": "slot-removed",
            "slot": "slot-a",
            "previous_revision": 0,
            "revision": 1,
            "record": archived_record,
            "evidence": {},
        }),
    );
    append_event(
        &events,
        9,
        &archived,
        "active-state-recorded",
        json!({
            "action": "slot-removed",
            "slot": "slot-a",
            "previous_revision": 0,
            "revision": 1,
            "previous_record_sha256": canonical_sha256(&record).expect("active digest"),
            "record": null,
            "evidence": {},
        }),
    );
    assert!(replay_stream(&events, None, |_| Ok(()))
        .expect("archive plus active removal closes lifecycle attempts")
        .pending_operations
        .is_empty());

    let mismatched = Scratch::new();
    let mismatched_events = mismatched.events();
    let first = import_log(
        &mismatched_events,
        vec![scoped_active_record("slot-a", 1)],
        vec![],
    );
    let second = append_event(
        &mismatched_events,
        2,
        &first,
        "operation-progress-recorded",
        json!({
            "slot": "slot-a",
            "operation": "finish",
            "journal": {"schema": 2, "kind": "finish", "machine": "node-a", "slot": "slot-a"},
        }),
    );
    append_event(
        &mismatched_events,
        3,
        &second,
        "operation-completed",
        json!({"slot": "slot-a", "operation": "create"}),
    );
    assert!(replay(&mismatched_events)
        .expect_err("mismatched completion must fail")
        .to_string()
        .contains("does not match"));

    let legacy_recovery = Scratch::new();
    let legacy_events = legacy_recovery.events();
    let first = import_log(
        &legacy_events,
        vec![scoped_active_record("slot-a", 1)],
        vec![],
    );
    let second = append_event(
        &legacy_events,
        2,
        &first,
        "recovery-started",
        json!({
            "slot": "slot-a",
            "operation": "finish",
            "actor": identity(12),
            "runner": identity(13),
            "handoff_writer": null,
            "coordinator_authorized": true,
        }),
    );
    append_event(
        &legacy_events,
        3,
        &second,
        "operation-completed",
        json!({"slot": "slot-a", "operation": "finish"}),
    );
    let pending = replay_stream(&legacy_events, None, |_| Ok(()))
        .expect("journal completion cannot claim a retained slot was recovered")
        .pending_operations;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, PendingOperationKind::Recovery);

    let naked = Scratch::new();
    let naked_events = naked.events();
    let first = import_log(
        &naked_events,
        vec![scoped_active_record("slot-a", 1)],
        vec![],
    );
    append_event(
        &naked_events,
        2,
        &first,
        "operation-completed",
        json!({"slot": "slot-a", "operation": "finish"}),
    );
    assert!(replay(&naked_events)
        .expect_err("naked completion must not clear an open attempt")
        .to_string()
        .contains("no pending progress or recovery"));
}

#[test]
fn historical_lifecycle_marker_shapes_replay_as_pending() {
    // Each fixture is the exact key shape emitted at the named Python writer
    // revision.  These are append-only historical formats, not permissive
    // optional-field combinations.
    let fixtures = [
        (
            "reclaim-started",
            include_str!("../tests/fixtures/reclaim-started-45b24f8.json"),
            PendingOperationKind::Reclaim,
        ),
        (
            "reclaim-started",
            include_str!("../tests/fixtures/reclaim-started-e5074d1.json"),
            PendingOperationKind::Reclaim,
        ),
        (
            "reclaim-started",
            include_str!("../tests/fixtures/reclaim-started-2a61b65.json"),
            PendingOperationKind::Reclaim,
        ),
        (
            "recovery-started",
            include_str!("../tests/fixtures/recovery-started-45b24f8.json"),
            PendingOperationKind::Recovery,
        ),
        (
            "recovery-started",
            include_str!("../tests/fixtures/recovery-started-ded39ae.json"),
            PendingOperationKind::Recovery,
        ),
    ];
    for (kind, source, expected_kind) in fixtures {
        let scratch = Scratch::new();
        let events = scratch.events();
        let first = import_log(&events, vec![scoped_active_record("slot-a", 1)], vec![]);
        let payload: Value = serde_json::from_str(source).expect("parse historical payload");
        append_event(&events, 2, &first, kind, payload);

        let replayed = replay_stream(&events, None, |_| Ok(()))
            .unwrap_or_else(|error| panic!("{kind} fixture did not replay: {error}"));
        assert_eq!(replayed.pending_operations.len(), 1, "{kind} fixture");
        let pending = &replayed.pending_operations[0];
        assert_eq!(pending.kind, expected_kind, "{kind} fixture");
        assert_eq!(pending.slot, "slot-a", "{kind} fixture");
        assert_eq!(pending.generation, Some(1), "{kind} fixture");
        assert_eq!(
            pending.operation.as_deref(),
            Some("finish"),
            "{kind} fixture"
        );
        let python = python_replay(&events);
        assert!(
            python.status.success(),
            "Python rejected {kind} historical fixture: {}",
            String::from_utf8_lossy(&python.stderr)
        );
    }

    for (kind, mut payload) in [
        (
            "reclaim-started",
            serde_json::from_str::<Value>(include_str!(
                "../tests/fixtures/reclaim-started-e5074d1.json"
            ))
            .expect("parse reclaim compatibility fixture"),
        ),
        (
            "recovery-started",
            serde_json::from_str::<Value>(include_str!(
                "../tests/fixtures/recovery-started-ded39ae.json"
            ))
            .expect("parse recovery compatibility fixture"),
        ),
    ] {
        payload
            .as_object_mut()
            .expect("fixture object")
            .remove("handoff_writer");
        let scratch = Scratch::new();
        let events = scratch.events();
        let first = import_log(&events, vec![scoped_active_record("slot-a", 1)], vec![]);
        append_event(&events, 2, &first, kind, payload);
        assert!(
            replay(&events)
                .expect_err("half-modern lifecycle shape must fail")
                .to_string()
                .contains("invalid fields"),
            "{kind} accepted an undocumented compatibility shape"
        );
    }
}

#[test]
fn pre_event_finish_recovery_closes_after_every_durable_snapshot_phase() {
    for snapshot_phase in ["before-archive", "archive-before-active", "after-active"] {
        let scratch = Scratch::new();
        let events = scratch.events();
        let active = scoped_active_record("slot-a", 1);
        let archived = archive_record("slot-a", 1);
        let imported_active = if snapshot_phase == "after-active" {
            Vec::new()
        } else {
            vec![active.clone()]
        };
        let imported_archive = if snapshot_phase == "before-archive" {
            Vec::new()
        } else {
            vec![archived.clone()]
        };
        let mut tip = import_log(&events, imported_active, imported_archive);
        tip = append_event(
            &events,
            2,
            &tip,
            "recovery-started",
            json!({
                "slot": "slot-a",
                "operation": "finish",
                "actor": identity(12),
                "runner": identity(13),
                "handoff_writer": null,
                "coordinator_authorized": true,
            }),
        );
        let mut sequence = 3;
        if snapshot_phase == "before-archive" {
            tip = append_event(
                &events,
                sequence,
                &tip,
                "archive-state-recorded",
                json!({
                    "action": "slot-removed",
                    "slot": "slot-a",
                    "previous_revision": 0,
                    "revision": 1,
                    "record": archived.clone(),
                    "evidence": {},
                }),
            );
            sequence += 1;
        }
        if snapshot_phase != "after-active" {
            tip = append_event(
                &events,
                sequence,
                &tip,
                "active-state-recorded",
                json!({
                    "action": "slot-removed",
                    "slot": "slot-a",
                    "previous_revision": 0,
                    "revision": 1,
                    "previous_record_sha256": canonical_sha256(&active)
                        .expect("active digest"),
                    "record": null,
                    "evidence": {},
                }),
            );
            sequence += 1;
        }

        let before_completion = replay_stream(&events, None, |_| Ok(()))
            .unwrap_or_else(|error| panic!("{snapshot_phase} did not reach cleanup: {error}"));
        assert_eq!(
            before_completion.summary.active_count, 0,
            "{snapshot_phase}"
        );
        assert_eq!(
            before_completion.summary.archive_count, 1,
            "{snapshot_phase}"
        );
        assert_eq!(
            before_completion.pending_operations.len(),
            1,
            "{snapshot_phase}"
        );
        let cleanup = &before_completion.pending_operations[0];
        assert_eq!(
            cleanup.kind,
            PendingOperationKind::Journal,
            "{snapshot_phase}"
        );
        assert_eq!(cleanup.slot, "slot-a", "{snapshot_phase}");
        assert_eq!(cleanup.generation, Some(1), "{snapshot_phase}");
        assert_eq!(
            cleanup.operation.as_deref(),
            Some("finish"),
            "{snapshot_phase}"
        );
        assert_eq!(
            cleanup.journal_path.as_deref(),
            Some("ACTIVE.node-a.journal"),
            "{snapshot_phase}"
        );
        assert_eq!(cleanup.journal_sha256, None, "{snapshot_phase}");

        append_event(
            &events,
            sequence,
            &tip,
            "operation-completed",
            json!({
                "slot": "slot-a",
                "operation": "finish",
                "journal_path": "ACTIVE.node-a.journal",
            }),
        );
        let completed = replay_stream(&events, None, |_| Ok(()))
            .unwrap_or_else(|error| panic!("{snapshot_phase} completion did not replay: {error}"));
        assert!(completed.pending_operations.is_empty(), "{snapshot_phase}");
        let python = python_replay(&events);
        assert!(
            python.status.success(),
            "Python rejected {snapshot_phase} historical finish: {}",
            String::from_utf8_lossy(&python.stderr)
        );
    }
}

#[test]
fn lifecycle_completion_requires_an_exact_generation_archive() {
    for archived_generation in [None, Some(2_u64)] {
        let scratch = Scratch::new();
        let events = scratch.events();
        let record = scoped_active_record("slot-a", 1);
        let mut tip = import_log(&events, vec![record.clone()], vec![]);
        tip = append_event(
            &events,
            2,
            &tip,
            "reclaim-started",
            json!({
                "slot": "slot-a",
                "generation": 1,
                "actor": identity(10),
                "runner": identity(11),
                "handoff_writer": null,
                "coordinator_authorized": true,
                "owner_state": "dead",
                "registered_liveness": "dead",
                "heartbeat_age_seconds": 601,
                "heartbeat_ttl_seconds": 600,
                "validate_complete": false,
                "live_validate_owner": false,
                "salvage_archive_root": null,
            }),
        );
        tip = append_event(
            &events,
            3,
            &tip,
            "operation-progress-recorded",
            json!({
                "slot": "slot-a",
                "operation": "finish",
                "journal": {
                    "schema": 2,
                    "kind": "finish",
                    "machine": "node-a",
                    "slot": "slot-a",
                    "record": record.clone(),
                },
            }),
        );
        tip = append_event(
            &events,
            4,
            &tip,
            "operation-completed",
            json!({"slot": "slot-a", "operation": "finish"}),
        );
        let mut sequence = 5;
        if let Some(generation) = archived_generation {
            tip = append_event(
                &events,
                sequence,
                &tip,
                "archive-state-recorded",
                json!({
                    "action": "slot-removed",
                    "slot": "slot-a",
                    "previous_revision": 0,
                    "revision": 1,
                    "record": archive_record("slot-a", generation),
                    "evidence": {},
                }),
            );
            sequence += 1;
        }
        append_event(
            &events,
            sequence,
            &tip,
            "active-state-recorded",
            json!({
                "action": "slot-removed",
                "slot": "slot-a",
                "previous_revision": 0,
                "revision": 1,
                "previous_record_sha256": canonical_sha256(&record).expect("active digest"),
                "record": null,
                "evidence": {},
            }),
        );
        let replayed = replay_stream(&events, None, |_| Ok(())).expect("replay unmatched archive");
        assert_eq!(replayed.pending_operations.len(), 1);
        assert_eq!(
            replayed.pending_operations[0].kind,
            PendingOperationKind::Reclaim
        );
    }
}

#[test]
fn malformed_lifecycle_marker_payloads_fail_closed() {
    for kind in [
        "operation-progress-recorded",
        "operation-completed",
        "reclaim-started",
        "recovery-started",
        "retirement-attempted",
    ] {
        let scratch = Scratch::new();
        let events = scratch.events();
        let first = one_event_log(&events);
        append_event(&events, 2, &first, kind, json!({}));
        let error = replay(&events)
            .expect_err("malformed lifecycle payload must fail")
            .to_string();
        assert!(error.contains("invalid fields"), "{kind}: {error}");
    }
}

#[test]
fn pending_only_python_create_is_blocking_in_policy_plan_and_explain(
) -> Result<(), crate::ObserverError> {
    let scratch = Scratch::new();
    let output = python_interrupted_create(&scratch.0);
    assert!(
        output.status.success(),
        "Python interrupted-create fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value =
        serde_json::from_slice(&output.stdout).expect("parse Python create fixture result");
    let events = PathBuf::from(oracle["events"].as_str().expect("events path"));
    let replayed = replay_stream(&events, None, |_| Ok(())).expect("replay Python create");
    assert_eq!(replayed.summary.active_count, 0);
    assert_eq!(replayed.pending_operations.len(), 1);
    assert_eq!(replayed.pending_operations[0].slot, "slot01");
    assert_eq!(
        replayed.pending_operations[0].kind,
        PendingOperationKind::Journal
    );

    let observed_at = (chrono::DateTime::parse_from_rfc3339(&replayed.tip_recorded_at)
        .expect("Python event-tip timestamp")
        - chrono::Duration::seconds(1))
    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let config = scratch.0.join("python-create-policy.json");
    let evidence = scratch.0.join("python-create-evidence.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!({
            "schema": 1,
            "machine": "testhost",
            "minimum_stale_seconds": 0,
            "max_plan_slots": 10,
            "evidence_max_age_seconds": 3155760000_u64,
            "maximum_census_seconds": 300,
            "maximum_future_skew_seconds": 60,
        }))
        .expect("encode Python create policy"),
    )
    .expect("write Python create policy");
    fs::write(
        &evidence,
        serde_json::to_vec(&json!({
            "schema": 1,
            "machine": "testhost",
            "event_tip_sha256": replayed.summary.tip_sha256,
            "census_started_at": observed_at,
            "observed_at": observed_at,
            "boot_id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "slots": [],
        }))
        .expect("encode Python create evidence"),
    )
    .expect("write Python create evidence");

    let direct = evaluate_policy_at(&events, &config, &evidence, &observed_at)?;
    assert_eq!(direct.len(), 1);
    assert_eq!(direct[0].slot, "slot01");
    assert_eq!(direct[0].verdict, Verdict::Blocked);
    assert_eq!(direct[0].generation, None);
    assert_eq!(direct[0].active_record_sha256, None);
    assert_eq!(direct[0].heartbeat_at, None);
    assert_eq!(direct[0].heartbeat_ttl_seconds, None);
    assert_eq!(
        direct[0].reason_codes,
        ["ACTIVE_RECORD_MISSING", "OPERATION_JOURNAL_PENDING"]
    );
    assert_eq!(
        direct[0].evaluated_at.as_deref(),
        Some(observed_at.as_str())
    );
    assert_eq!(
        direct[0].evidence_age_at_evaluation_nanoseconds.as_deref(),
        Some("0")
    );

    let plain_index = scratch.0.join("pending-create-without-policy.sqlite");
    rebuild_index(&events, &plain_index)?;
    let plain_explained = read_decision(&plain_index, "slot01")?;
    assert_eq!(plain_explained.verdict, Verdict::Blocked);
    assert_eq!(plain_explained.generation, None);
    assert_eq!(plain_explained.active_record_sha256, None);
    assert_eq!(
        plain_explained.reason_codes,
        [
            "POLICY_INPUTS_MISSING",
            "ACTIVE_RECORD_MISSING",
            "OPERATION_JOURNAL_PENDING",
        ]
    );
    let plain_plan = read_pressure_plan(&plain_index, 1, None)?;
    assert!(plain_plan.eligible.is_empty());
    assert!(plain_plan.deferred_eligible.is_empty());
    assert!(plain_plan.unknown.is_empty());
    assert_eq!(plain_plan.blocked, [plain_explained]);
    assert!(!plain_plan.target_met);

    let schema_one_index = scratch.0.join("pending-create-schema-one.sqlite");
    write_schema_one_index(&events, &schema_one_index);
    let schema_one_explained = read_decision(&schema_one_index, "slot01")?;
    assert_eq!(schema_one_explained.verdict, Verdict::Blocked);
    assert_eq!(schema_one_explained.generation, None);
    assert_eq!(schema_one_explained.active_record_sha256, None);
    assert_eq!(
        schema_one_explained.reason_codes,
        [
            "LEGACY_INDEX_SCHEMA",
            "ACTIVE_RECORD_MISSING",
            "OPERATION_JOURNAL_PENDING",
        ]
    );
    let schema_one_plan = read_pressure_plan(&schema_one_index, 1, None)?;
    assert!(schema_one_plan.eligible.is_empty());
    assert!(schema_one_plan.deferred_eligible.is_empty());
    assert!(schema_one_plan.unknown.is_empty());
    assert_eq!(schema_one_plan.blocked, [schema_one_explained]);
    assert!(!schema_one_plan.target_met);

    let index = scratch.0.join("pending-create.sqlite");
    rebuild_policy_index(&events, &index, &config, &evidence)?;
    let explained = read_decision(&index, "slot01")?;
    assert_eq!(explained.slot, direct[0].slot);
    assert_eq!(explained.verdict, direct[0].verdict);
    assert_eq!(explained.reason_codes, direct[0].reason_codes);
    assert_eq!(explained.generation, None);
    assert_eq!(explained.active_record_sha256, None);
    assert_eq!(explained.heartbeat_at, None);
    assert_eq!(explained.heartbeat_ttl_seconds, None);
    // Rebuild intentionally captures its own later evaluation instant; explain
    // must reproduce that indexed instant rather than the direct call above.
    assert_ne!(explained.evaluated_at, direct[0].evaluated_at);
    assert!(
        explained
            .evidence_age_at_evaluation_nanoseconds
            .as_deref()
            .expect("indexed evidence age")
            .parse::<u128>()
            .expect("numeric indexed evidence age")
            >= 1_000_000_000
    );
    let plan = read_pressure_plan(&index, 1, None)?;
    assert!(plan.eligible.is_empty());
    assert!(plan.deferred_eligible.is_empty());
    assert!(plan.unknown.is_empty());
    assert_eq!(plan.blocked.len(), 1);
    assert_eq!(plan.blocked[0], explained);
    assert!(!plan.target_met);
    Ok(())
}

#[test]
fn real_python_interrupted_finish_is_pending_in_rust() -> Result<(), crate::ObserverError> {
    let scratch = Scratch::new();
    let output = python_interrupted_finish(&scratch.0);
    assert!(
        output.status.success(),
        "Python interrupted-finish fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value =
        serde_json::from_slice(&output.stdout).expect("parse Python fixture result");
    let events = PathBuf::from(oracle["events"].as_str().expect("events path"));
    let replayed = replay_stream(&events, None, |_| Ok(())).expect("replay Python finish");
    let pending = replayed
        .pending_operations
        .iter()
        .find(|pending| pending.kind == crate::replay::PendingOperationKind::Journal)
        .expect("Python finish remains pending");
    assert_eq!(pending.slot, oracle["slot"].as_str().expect("slot"));
    assert_eq!(pending.generation, oracle["generation"].as_u64());
    assert_eq!(pending.operation.as_deref(), oracle["kind"].as_str());
    assert_eq!(pending.journal_path.as_deref(), oracle["path"].as_str());
    assert_eq!(
        pending.journal_sha256.as_deref(),
        oracle["journal_sha256"].as_str()
    );

    let record = replayed.active_records["slot01"].clone();
    let observed_at = replayed.tip_recorded_at.clone();
    let config = scratch.0.join("python-policy.json");
    let evidence = scratch.0.join("python-evidence.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!({
            "schema": 1,
            "machine": "testhost",
            "minimum_stale_seconds": 0,
            "max_plan_slots": 10,
            "evidence_max_age_seconds": 3155760000_u64,
            "maximum_census_seconds": 300,
            "maximum_future_skew_seconds": 60,
        }))
        .expect("encode Python fixture policy"),
    )
    .expect("write Python fixture policy");
    fs::write(
        &evidence,
        serde_json::to_vec(&json!({
            "schema": 1,
            "machine": "testhost",
            "event_tip_sha256": replayed.summary.tip_sha256,
            "census_started_at": observed_at,
            "observed_at": observed_at,
            "boot_id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "slots": [{
                "slot": "slot01",
                "generation": 1,
                "active_record_sha256": canonical_sha256(&record)?,
                "scope": null,
                "checkouts": [{
                    "name": record["checkouts"][0]["name"],
                    "path": record["checkouts"][0]["path"],
                    "device": 10,
                    "inode": 20,
                    "mount_id": 30,
                    "directory": true,
                    "symlink_free": true,
                    "mount_stable": true,
                }],
                "journal": "absent",
                "process_use": "unused",
                "reclaimable_bytes": 4096,
            }],
        }))
        .expect("encode Python fixture evidence"),
    )
    .expect("write Python fixture evidence");
    let decision = &evaluate_policy_at(&events, &config, &evidence, &observed_at)?[0];
    assert_eq!(decision.verdict, Verdict::Blocked);
    assert!(decision
        .reason_codes
        .iter()
        .any(|reason| reason == "OPERATION_JOURNAL_PENDING"));
    assert!(decision
        .reason_codes
        .iter()
        .any(|reason| reason == "RECLAIM_PENDING"));
    assert!(decision
        .reason_codes
        .iter()
        .any(|reason| reason == "JOURNAL_EVENT_EVIDENCE_CONFLICT"));
    Ok::<(), crate::ObserverError>(())
}

#[test]
fn real_python_late_refusal_completion_preserves_all_attempt_markers() {
    for (mode, expected) in [
        (
            "retirement",
            BTreeSet::from([
                PendingOperationKind::Reclaim,
                PendingOperationKind::Retirement,
            ]),
        ),
        (
            "recovery",
            BTreeSet::from([
                PendingOperationKind::Reclaim,
                PendingOperationKind::Recovery,
            ]),
        ),
    ] {
        let scratch = Scratch::new();
        let output = python_late_refusal_after_journal_completion(&scratch.0, mode);
        assert!(
            output.status.success(),
            "Python {mode} late-refusal fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let oracle: Value =
            serde_json::from_slice(&output.stdout).expect("parse Python late-refusal result");
        let events = PathBuf::from(oracle["events"].as_str().expect("events path"));
        let replayed = replay_stream(&events, None, |_| Ok(())).expect("replay Python refusal");
        assert_eq!(
            (
                replayed.summary.active_count,
                replayed.summary.archive_count
            ),
            (1, 0),
            "{mode} must retain the slot"
        );
        assert_eq!(
            replayed
                .pending_operations
                .iter()
                .map(|pending| pending.kind)
                .collect::<BTreeSet<_>>(),
            expected,
            "{mode} completion erased a nonterminal lifecycle attempt"
        );
        assert!(replayed
            .pending_operations
            .iter()
            .all(|pending| pending.kind != PendingOperationKind::Journal));
    }
}

#[test]
fn completed_scoped_journal_paths_can_be_reused_by_real_python_retries() {
    for operation in ["create", "finish"] {
        let scratch = Scratch::new();
        let output = python_scoped_journal_retry(&scratch.0, operation);
        assert!(
            output.status.success(),
            "Python {operation} retry fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let oracle: Value =
            serde_json::from_slice(&output.stdout).expect("parse Python retry fixture result");
        let events = PathBuf::from(oracle["events"].as_str().expect("events path"));
        let replayed = replay_stream(&events, None, |_| Ok(()))
            .unwrap_or_else(|error| panic!("replay Python {operation} retry: {error}"));
        let journals = replayed
            .pending_operations
            .iter()
            .filter(|pending| pending.kind == PendingOperationKind::Journal)
            .collect::<Vec<_>>();
        assert_eq!(journals.len(), 1, "{operation} retry");
        assert_eq!(journals[0].slot, "slot01", "{operation} retry");
        assert_eq!(
            journals[0].operation.as_deref(),
            Some(operation),
            "{operation} retry"
        );
        assert_eq!(
            journals[0].journal_path.as_deref(),
            oracle["path"].as_str(),
            "{operation} retry"
        );
    }
}

#[test]
fn shadow_policy_ttl_boundary_and_scope_liveness_fail_closed() {
    let at_boundary = Scratch::new();
    let decision = policy_decision(
        &at_boundary,
        scoped_active_record("slot-a", 1),
        "2026-09-22T09:40:00+00:00",
        |_| {},
    );
    assert_eq!(decision.verdict, Verdict::Blocked);
    assert!(decision
        .reason_codes
        .iter()
        .any(|reason| reason == "HEARTBEAT_TTL_ACTIVE"));

    let expired = Scratch::new();
    let expired = policy_decision(
        &expired,
        scoped_active_record("slot-a", 1),
        "2026-09-22T09:40:01+00:00",
        |_| {},
    );
    assert_rollout_gated(&expired);
    assert!(!expired
        .reason_codes
        .iter()
        .any(|reason| reason == "HEARTBEAT_TTL_ACTIVE"));

    for (state, leader, expected, reason) in [
        ("live", "same", Verdict::Blocked, "TASK_SCOPE_LIVE"),
        (
            "unknown",
            "unknown",
            Verdict::Unknown,
            "TASK_SCOPE_STATE_UNKNOWN",
        ),
        (
            "dead",
            "reused",
            Verdict::Unknown,
            "TASK_SCOPE_RUNTIME_UNVERIFIED",
        ),
    ] {
        let scratch = Scratch::new();
        let decision = policy_decision(
            &scratch,
            scoped_active_record("slot-a", 1),
            "2026-09-22T09:40:01+00:00",
            |value| {
                value["slots"][0]["scope"]["state"] = Value::String(state.to_owned());
                value["slots"][0]["scope"]["leader_state"] = Value::String(leader.to_owned());
            },
        );
        assert_eq!(decision.verdict, expected);
        if !reason.is_empty() {
            assert!(decision.reason_codes.iter().any(|item| item == reason));
        }
    }
}

#[test]
fn shadow_policy_uses_precise_bounded_census_time_for_all_age_boundaries(
) -> Result<(), crate::ObserverError> {
    let ttl_boundary = direct_policy_decision(
        "2026-09-22T09:40:00+00:00",
        "2026-09-22T09:40:00+00:00",
        "2026-09-22T09:40:00+00:00",
        0,
        60,
        300,
        60,
    )
    .expect("exact TTL boundary is valid");
    assert_eq!(ttl_boundary.verdict, Verdict::Blocked);
    assert!(ttl_boundary
        .reason_codes
        .iter()
        .any(|reason| reason == "HEARTBEAT_TTL_ACTIVE"));
    assert_eq!(
        ttl_boundary
            .evidence_age_at_evaluation_nanoseconds
            .as_deref(),
        Some("0")
    );

    let after_ttl = direct_policy_decision(
        "2026-09-22T09:40:00.000000001+00:00",
        "2026-09-22T09:40:00.000000001+00:00",
        "2026-09-22T09:40:00.000000001+00:00",
        0,
        60,
        300,
        60,
    )?;
    assert_rollout_gated(&after_ttl);
    assert!(!after_ttl
        .reason_codes
        .iter()
        .any(|reason| reason == "HEARTBEAT_TTL_ACTIVE"));

    let stale_boundary = direct_policy_decision(
        "2026-09-22T09:45:00+00:00",
        "2026-09-22T09:45:00+00:00",
        "2026-09-22T09:45:00+00:00",
        900,
        60,
        300,
        60,
    )
    .expect("exact minimum-stale boundary is valid");
    assert!(stale_boundary
        .reason_codes
        .iter()
        .any(|reason| reason == "MINIMUM_STALE_AGE_ACTIVE"));
    let after_stale = direct_policy_decision(
        "2026-09-22T09:45:00.000000001+00:00",
        "2026-09-22T09:45:00.000000001+00:00",
        "2026-09-22T09:45:00.000000001+00:00",
        900,
        60,
        300,
        60,
    )?;
    assert_rollout_gated(&after_stale);
    assert!(!after_stale
        .reason_codes
        .iter()
        .any(|reason| reason == "MINIMUM_STALE_AGE_ACTIVE"));

    let future_boundary = direct_policy_decision(
        "2026-09-22T09:41:00+00:00",
        "2026-09-22T09:41:00+00:00",
        "2026-09-22T09:40:00+00:00",
        0,
        60,
        300,
        60,
    )
    .expect("exact future-skew boundary is valid");
    assert!(future_boundary
        .reason_codes
        .iter()
        .any(|reason| reason == "HEARTBEAT_TTL_ACTIVE"));
    assert!(direct_policy_decision(
        "2026-09-22T09:41:00.000000001+00:00",
        "2026-09-22T09:41:00.000000001+00:00",
        "2026-09-22T09:40:00+00:00",
        0,
        60,
        300,
        60,
    )
    .expect_err("future skew plus one nanosecond must fail")
    .to_string()
    .contains("too far in the future"));

    let max_age_boundary = direct_policy_decision(
        "2026-09-22T09:39:00+00:00",
        "2026-09-22T09:39:00+00:00",
        "2026-09-22T09:40:00+00:00",
        0,
        60,
        300,
        60,
    )
    .expect("exact maximum evidence age is valid");
    assert_eq!(
        max_age_boundary
            .evidence_age_at_evaluation_nanoseconds
            .as_deref(),
        Some("60000000000")
    );
    assert!(direct_policy_decision(
        "2026-09-22T09:38:59.999999999+00:00",
        "2026-09-22T09:38:59.999999999+00:00",
        "2026-09-22T09:40:00+00:00",
        0,
        60,
        300,
        60,
    )
    .expect_err("maximum age plus one nanosecond must fail")
    .to_string()
    .contains("maximum age"));

    direct_policy_decision(
        "2026-09-22T09:35:00+00:00",
        "2026-09-22T09:40:00+00:00",
        "2026-09-22T09:40:00+00:00",
        0,
        60,
        300,
        60,
    )
    .expect("exact census duration is valid");
    assert!(direct_policy_decision(
        "2026-09-22T09:34:59.999999999+00:00",
        "2026-09-22T09:40:00+00:00",
        "2026-09-22T09:40:00+00:00",
        0,
        60,
        300,
        60,
    )
    .expect_err("census duration plus one nanosecond must fail")
    .to_string()
    .contains("census window"));

    direct_policy_decision(
        "2026-09-22T09:34:01+00:00",
        "2026-09-22T09:34:01+00:00",
        "2026-09-22T09:40:00+00:00",
        0,
        3600,
        300,
        60,
    )
    .expect("event-tip skew boundary is valid");
    assert!(direct_policy_decision(
        "2026-09-22T09:34:00.999999999+00:00",
        "2026-09-22T09:34:00.999999999+00:00",
        "2026-09-22T09:40:00+00:00",
        0,
        3600,
        300,
        60,
    )
    .expect_err("event-tip skew plus one nanosecond must fail")
    .to_string()
    .contains("event tip"));
    Ok(())
}

#[test]
fn shadow_policy_rejects_chrono_unrepresentable_freshness_without_panicking() {
    let too_large = i64::MAX as u64;
    for (maximum_age, census, future) in [
        (too_large, 300, 60),
        (too_large, too_large, 60),
        (too_large, 300, too_large),
    ] {
        let error = direct_policy_decision(
            "2026-09-22T09:40:00+00:00",
            "2026-09-22T09:40:00+00:00",
            "2026-09-22T09:40:00+00:00",
            0,
            maximum_age,
            census,
            future,
        )
        .expect_err("chrono-unrepresentable policy duration must be rejected");
        assert!(error.to_string().contains("representable"), "{error}");
    }
}

#[test]
fn shadow_policy_distinguishes_reboot_pid_reuse_and_identity_drift() {
    for (scope_state, expect_reboot_conflict) in [("dead", false), ("live", true)] {
        let reboot = Scratch::new();
        let decision = policy_decision(
            &reboot,
            scoped_active_record("slot-a", 1),
            "2026-09-22T09:40:01+00:00",
            |value| {
                value["boot_id"] = Value::String("bbbbbbbb-cccc-dddd-eeee-ffffffffffff".to_owned());
                value["slots"][0]["scope"]["state"] = Value::String(scope_state.to_owned());
                value["slots"][0]["scope"]["leader_state"] = Value::String("reused".to_owned());
            },
        );
        if scope_state == "dead" {
            assert_rollout_gated(&decision);
        } else {
            assert_eq!(decision.verdict, Verdict::Blocked);
            assert!(decision
                .reason_codes
                .iter()
                .any(|reason| reason == "TASK_SCOPE_LIVE"));
        }
        assert_eq!(
            decision
                .reason_codes
                .iter()
                .any(|reason| reason == "REBOOT_SCOPE_STATE_CONFLICT"),
            expect_reboot_conflict,
            "unexpected reboot conflict result for scope state {scope_state}: {:?}",
            decision.reason_codes
        );
    }

    for (field, reason) in [
        ("generation", "GENERATION_DRIFT"),
        ("active_record_sha256", "ACTIVE_RECORD_DIGEST_DRIFT"),
    ] {
        let scratch = Scratch::new();
        let decision = policy_decision(
            &scratch,
            scoped_active_record("slot-a", 1),
            "2026-09-22T09:40:01+00:00",
            |value| {
                value["slots"][0][field] = if field == "generation" {
                    json!(2)
                } else {
                    Value::String("f".repeat(64))
                };
            },
        );
        assert_eq!(decision.verdict, Verdict::Unknown);
        assert!(decision.reason_codes.iter().any(|item| item == reason));
    }

    let scope_drift = Scratch::new();
    let decision = policy_decision(
        &scope_drift,
        scoped_active_record("slot-a", 1),
        "2026-09-22T09:40:01+00:00",
        |value| {
            value["slots"][0]["scope"]["recorded"]["invocation_id"] = Value::String("b".repeat(32));
        },
    );
    assert_eq!(decision.verdict, Verdict::Unknown);
    assert!(decision
        .reason_codes
        .iter()
        .any(|reason| reason == "SCOPE_IDENTITY_DRIFT"));
}

#[test]
fn indexed_config_and_record_drift_invalidates_a_captured_decision() {
    for (statement, message) in [
        (
            "UPDATE observer_metadata SET config_sha256 = 'ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff'",
            "canonical digest",
        ),
        (
            "UPDATE active_records SET record_sha256 = 'ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff' WHERE slot = 'slot-a'",
            "active projection",
        ),
    ] {
        let scratch = Scratch::new();
        let events = scratch.events();
        let record = scoped_active_record("slot-a", 1);
        import_log(&events, vec![record.clone()], vec![]);
        let (config, evidence) =
            write_policy_inputs(&scratch, &[record], "2026-09-22T09:40:01+00:00", |_| {});
        let index = scratch.0.join("drift.sqlite");
        rebuild_policy_index(&events, &index, &config, &evidence).expect("build bound index");
        let connection = Connection::open(&index).expect("open bound index");
        connection.execute(statement, []).expect("inject indexed drift");
        drop(connection);
        let error = read_decision(&index, "slot-a")
            .expect_err("digest drift must invalidate decision")
            .to_string();
        assert!(error.contains(message), "unexpected error: {error}");
    }
}

#[test]
fn indexed_decisions_are_fully_recomputed_and_holds_cannot_be_hidden() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = scoped_active_record("slot-a", 1);
    let first = import_log(&events, vec![record.clone()], vec![]);
    append_event(
        &events,
        2,
        &first,
        "slot-held",
        json!({"slot": "slot-a", "generation": 1, "reason": "retain"}),
    );
    let (config, evidence) = write_policy_inputs(
        &scratch,
        &[record],
        "2026-09-22T09:40:01.123456789+00:00",
        |_| {},
    );
    let index = scratch.0.join("decision-integrity.sqlite");
    rebuild_policy_index(&events, &index, &config, &evidence).expect("build held policy index");

    let connection = Connection::open(&index).expect("open policy index");
    let encoded: String = connection
        .query_row(
            "SELECT decision_json FROM policy_decisions WHERE slot = 'slot-a'",
            [],
            |row| row.get(0),
        )
        .expect("read held decision");
    let mut forged: Value = serde_json::from_str(&encoded).expect("parse held decision");
    forged["verdict"] = Value::String("ELIGIBLE".to_owned());
    forged["reason_codes"] = json!([]);
    forged["reclaimable_bytes"] = Value::String("18446744073709551615".to_owned());
    connection
        .execute(
            "UPDATE policy_decisions SET verdict = 'ELIGIBLE', decision_json = ?1
             WHERE slot = 'slot-a'",
            [serde_json::to_string(&forged).expect("encode forged decision")],
        )
        .expect("forge decision");
    drop(connection);
    let error = read_decision(&index, "slot-a")
        .expect_err("full decision forgery must fail")
        .to_string();
    assert!(
        error.contains("canonical re-evaluation"),
        "unexpected error: {error}"
    );

    // The stored JSON alone must also match: once with a changed decision under
    // an unchanged verdict column, once with the same decision re-encoded.
    type Forgery = fn(&str) -> String;
    let forgeries: [(&str, Forgery); 2] = [
        ("changed reasons", |stored| {
            let mut forged: Value = serde_json::from_str(stored).expect("parse stored decision");
            assert_ne!(forged["reason_codes"], json!(["ACTIVE_HOLD"]));
            forged["reason_codes"] = json!(["ACTIVE_HOLD"]);
            serde_json::to_string(&forged).expect("encode reasons-only forgery")
        }),
        ("re-encoded decision", |stored| {
            let decision: crate::policy::PolicyDecision =
                serde_json::from_str(stored).expect("decode stored decision");
            let forged = serde_json::to_string_pretty(&decision).expect("re-encode decision");
            assert_ne!(forged, stored);
            forged
        }),
    ];
    for (case, forge) in forgeries {
        rebuild_policy_index(&events, &index, &config, &evidence).expect("restore policy index");
        let connection = Connection::open(&index).expect("reopen policy index");
        let stored: String = connection
            .query_row(
                "SELECT decision_json FROM policy_decisions WHERE slot = 'slot-a'",
                [],
                |row| row.get(0),
            )
            .expect("read restored decision");
        read_decision(&index, "slot-a").expect("restored decision reads");
        connection
            .execute(
                "UPDATE policy_decisions SET decision_json = ?1 WHERE slot = 'slot-a'",
                [forge(&stored)],
            )
            .expect("forge stored decision JSON");
        drop(connection);
        let error = read_decision(&index, "slot-a")
            .err()
            .unwrap_or_else(|| panic!("{case} must fail"))
            .to_string();
        assert!(error.contains("canonical re-evaluation"), "{case}: {error}");
    }

    rebuild_policy_index(&events, &index, &config, &evidence).expect("restore policy index");
    let connection = Connection::open(&index).expect("reopen policy index");
    connection
        .execute("DELETE FROM holds WHERE slot = 'slot-a'", [])
        .expect("hide hold projection");
    drop(connection);
    let error = read_decision(&index, "slot-a")
        .expect_err("hidden active hold must fail")
        .to_string();
    assert!(
        error.contains("hold projection"),
        "unexpected error: {error}"
    );

    rebuild_policy_index(&events, &index, &config, &evidence).expect("restore policy index again");
    let connection = Connection::open(&index).expect("reopen policy index again");
    connection
        .execute("DELETE FROM policy_decisions WHERE slot = 'slot-a'", [])
        .expect("remove covered decision");
    drop(connection);
    let error = read_decision(&index, "slot-a")
        .expect_err("missing policy decision must fail")
        .to_string();
    assert!(
        error.contains("cover every active row"),
        "unexpected error: {error}"
    );
}

#[test]
fn evidence_tip_window_age_and_replacement_are_fail_closed() {
    for (case, expected, change_config, change_evidence) in [
        (
            "tip",
            "event tip",
            None,
            Some(("event_tip_sha256", Value::String("f".repeat(64)))),
        ),
        (
            "census",
            "census window",
            None,
            Some((
                "census_started_at",
                Value::String("2026-09-22T09:35:00.999999999+00:00".to_owned()),
            )),
        ),
        (
            "stale",
            "maximum age",
            Some(("evidence_max_age_seconds", json!(1))),
            Some((
                "observed_at",
                Value::String("2026-09-22T09:35:02+00:00".to_owned()),
            )),
        ),
        (
            "future",
            "too far in the future",
            Some(("maximum_future_skew_seconds", json!(1))),
            Some((
                "observed_at",
                Value::String("9999-01-01T00:00:00+00:00".to_owned()),
            )),
        ),
    ] {
        let scratch = Scratch::new();
        let events = scratch.events();
        let record = scoped_active_record("slot-a", 1);
        import_log(&events, vec![record.clone()], vec![]);
        let (config, evidence) =
            write_policy_inputs(&scratch, &[record], "2026-09-22T09:40:01+00:00", |_| {});
        if let Some((field, value)) = change_config {
            rewrite_json(&config, |document| {
                document[field] = value;
                if case == "stale" {
                    document["maximum_future_skew_seconds"] = json!(1);
                }
            });
        }
        if let Some((field, value)) = change_evidence {
            rewrite_json(&evidence, |document| {
                document[field] = value;
                if case == "stale" || case == "future" {
                    document["census_started_at"] = document["observed_at"].clone();
                }
            });
        }
        let error = rebuild_policy_index(
            &events,
            &scratch.0.join(format!("{case}.sqlite")),
            &config,
            &evidence,
        )
        .expect_err("unbound or non-fresh evidence must fail")
        .to_string();
        assert!(error.contains(expected), "case {case}: {error}");
    }

    let scratch = Scratch::new();
    let events = scratch.events();
    let record = scoped_active_record("slot-a", 1);
    import_log(&events, vec![record.clone()], vec![]);
    let (new_config, new_evidence) = write_policy_inputs(
        &scratch,
        std::slice::from_ref(&record),
        "2026-09-22T09:40:02+00:00",
        |_| {},
    );
    let index = scratch.0.join("monotonic.sqlite");
    rebuild_policy_index(&events, &index, &new_config, &new_evidence)
        .expect("build newer evidence index");
    let (old_config, old_evidence) = write_policy_inputs(
        &scratch,
        std::slice::from_ref(&record),
        "2026-09-22T09:40:01+00:00",
        |_| {},
    );
    let error = rebuild_policy_index(&events, &index, &old_config, &old_evidence)
        .expect_err("older evidence must not replace newer decisions")
        .to_string();
    assert!(error.contains("older or equivocal evidence"), "{error}");
    let error = rebuild_index(&events, &index)
        .expect_err("unbound rebuild must not erase the evidence high-water mark")
        .to_string();
    assert!(error.contains("high-water mark"), "{error}");

    rewrite_json(&new_evidence, |value| {
        value["census_started_at"] = Value::String("2026-09-22T09:40:02+00:00".to_owned());
        value["observed_at"] = Value::String("2026-09-22T09:40:02+00:00".to_owned());
    });

    let connection = Connection::open(&index).expect("open policy index for regression probe");
    connection
        .execute(
            "UPDATE observer_metadata SET evaluated_at = '9999-01-01T00:00:00+00:00'",
            [],
        )
        .expect("install future evaluation high-water mark");
    drop(connection);
    let error = rebuild_policy_index(&events, &index, &new_config, &new_evidence)
        .expect_err("evaluation time must not regress")
        .to_string();
    assert!(error.contains("evaluated_at regresses"), "{error}");
}

#[test]
fn explain_and_plan_recheck_captured_evidence_age_at_read_time() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = scoped_active_record("slot-a", 1);
    import_log(&events, vec![record.clone()], vec![]);
    let observed_at = "2026-09-22T09:40:01+00:00";
    let (config, evidence) = write_policy_inputs(&scratch, &[record], observed_at, |_| {});
    let index = scratch.0.join("read-age.sqlite");
    rebuild_policy_index(&events, &index, &config, &evidence)
        .expect("build policy index before evidence expires");

    let evaluated_at = read_decision(&index, "slot-a")
        .expect("read freshly evaluated decision")
        .evaluated_at
        .expect("policy decision has evaluated_at");
    let before_evaluation = chrono::DateTime::parse_from_rfc3339(&evaluated_at)
        .expect("parse evaluated_at")
        - chrono::Duration::nanoseconds(1);
    let before_evaluation = before_evaluation.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    for error in [
        read_decision_at(&index, "slot-a", &before_evaluation)
            .expect_err("explain must reject wall-clock rollback"),
        read_pressure_plan_at(&index, 0, None, &before_evaluation)
            .expect_err("plan must reject wall-clock rollback"),
    ] {
        assert!(error.to_string().contains("clock rollback"), "{error}");
    }

    let observed = chrono::DateTime::parse_from_rfc3339(observed_at).expect("parse fixture time");
    let exact_boundary = observed + chrono::Duration::seconds(3_155_760_000);
    let expired = exact_boundary + chrono::Duration::nanoseconds(1);
    let exact_boundary = exact_boundary.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let expired = expired.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);

    read_decision_at(&index, "slot-a", &exact_boundary)
        .expect("explain accepts the exact evidence-age boundary");
    read_pressure_plan_at(&index, 0, None, &exact_boundary)
        .expect("plan accepts the exact evidence-age boundary");
    for error in [
        read_decision_at(&index, "slot-a", &expired)
            .expect_err("explain must reject evidence one nanosecond past max age"),
        read_pressure_plan_at(&index, 0, None, &expired)
            .expect_err("plan must reject evidence one nanosecond past max age"),
    ] {
        assert!(error.to_string().contains("maximum age"), "{error}");
    }
}

#[test]
fn known_blockers_dominate_unknown_evidence() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = scoped_active_record("slot-a", 1);
    let first = import_log(&events, vec![record.clone()], vec![]);
    append_event(
        &events,
        2,
        &first,
        "slot-held",
        json!({"slot": "slot-a", "generation": 1, "reason": "retain"}),
    );
    let (config, evidence) =
        write_policy_inputs(&scratch, &[record], "2026-09-22T09:40:01+00:00", |value| {
            value["slots"][0]["process_use"] = Value::String("unknown".to_owned())
        });
    let index = scratch.0.join("severity.sqlite");
    rebuild_policy_index(&events, &index, &config, &evidence).expect("build mixed decision");
    let decision = read_decision(&index, "slot-a").expect("read mixed decision");
    assert_eq!(decision.verdict, Verdict::Blocked);
    assert!(decision
        .reason_codes
        .iter()
        .any(|reason| reason == "ACTIVE_HOLD"));
    assert!(decision
        .reason_codes
        .iter()
        .any(|reason| reason == "PROCESS_USE_UNKNOWN"));
}

#[test]
fn shadow_policy_refuses_symlink_mount_journal_and_process_use_evidence() {
    let cases: [(&str, &str, Value); 4] = [
        (
            "symlink_free",
            "CHECKOUT_SYMLINK_COMPONENT",
            Value::Bool(false),
        ),
        (
            "mount_stable",
            "CHECKOUT_MOUNT_CROSSING",
            Value::Bool(false),
        ),
        (
            "journal",
            "JOURNAL_PRESENT",
            Value::String("present".to_owned()),
        ),
        (
            "process_use",
            "PROCESS_USES_SLOT",
            Value::String("in-use".to_owned()),
        ),
    ];
    for (field, reason, replacement) in cases {
        let scratch = Scratch::new();
        let decision = policy_decision(
            &scratch,
            scoped_active_record("slot-a", 1),
            "2026-09-22T09:40:01+00:00",
            |value| {
                if matches!(field, "journal" | "process_use") {
                    value["slots"][0][field] = replacement.clone();
                } else {
                    value["slots"][0]["checkouts"][0][field] = replacement.clone();
                }
            },
        );
        assert_eq!(decision.verdict, Verdict::Blocked, "case {field}");
        assert!(decision.reason_codes.iter().any(|item| item == reason));
    }
}

#[test]
fn legacy_rows_are_readable_but_never_policy_eligible() {
    let scratch = Scratch::new();
    let events = scratch.events();
    one_event_log(&events);
    let index = scratch.0.join("legacy-readable.sqlite");
    rebuild_index(&events, &index).expect("build unbound observer index");
    let decision = read_decision(&index, "slot-a").expect("read legacy active row");
    assert_eq!(decision.verdict, Verdict::Unknown);
    assert_eq!(decision.reason_codes, ["POLICY_INPUTS_MISSING"]);

    let policy = read_pressure_plan(&index, 1, None).expect("plan over legacy row");
    assert!(policy.eligible.is_empty());
    assert_eq!(policy.unknown.len(), 1);

    let (config, evidence) = write_policy_inputs(
        &scratch,
        &[active_record("slot-a", 1)],
        "2026-09-22T09:40:01+00:00",
        |_| {},
    );
    rebuild_policy_index(&events, &index, &config, &evidence)
        .expect("policy index retains legacy active row");
    let decision = read_decision(&index, "slot-a").expect("read legacy policy decision");
    assert_eq!(decision.verdict, Verdict::Unknown);
    assert_eq!(
        decision.reason_codes,
        [
            "TASKGRAPH_CLAIM_UNVERIFIED",
            "LEGACY_SCOPE_IDENTITY_MISSING"
        ]
    );
}

fn write_schema_one_index(events: &Path, index: &Path) {
    let replayed = replay_stream(events, None, |_| Ok(())).expect("replay schema-one fixture");
    let connection = Connection::open(index).expect("create schema-one index");
    connection
        .execute_batch(
            "CREATE TABLE observer_metadata (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                schema_version INTEGER NOT NULL, machine TEXT NOT NULL,
                replay_count INTEGER NOT NULL, tip_sha256 TEXT NOT NULL,
                active_revision TEXT NOT NULL, active_count INTEGER NOT NULL,
                archive_revision TEXT NOT NULL, archive_count INTEGER NOT NULL
             );
             CREATE TABLE event_log (
                sequence INTEGER PRIMARY KEY, machine TEXT NOT NULL,
                previous_sha256 TEXT NOT NULL, recorded_at TEXT NOT NULL,
                kind TEXT NOT NULL, payload_json TEXT NOT NULL,
                sha256 TEXT NOT NULL UNIQUE
             );
             CREATE TABLE active_records (slot TEXT PRIMARY KEY, record_json TEXT NOT NULL);
             CREATE TABLE archive_records (
                archive_id TEXT PRIMARY KEY, slot TEXT NOT NULL UNIQUE,
                record_json TEXT NOT NULL
             );",
        )
        .expect("create legacy schema");
    connection
        .execute(
            "INSERT INTO observer_metadata VALUES (1, 1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                replayed.summary.machine,
                replayed.summary.replay_count,
                replayed.summary.tip_sha256,
                replayed.summary.active_revision.to_string(),
                replayed.summary.active_count,
                replayed.summary.archive_revision.to_string(),
                replayed.summary.archive_count,
            ],
        )
        .expect("insert legacy metadata");
    for entry in fs::read_dir(events).expect("list schema-one source events") {
        let event: Value = serde_json::from_slice(
            &fs::read(entry.expect("read event entry").path()).expect("read event file"),
        )
        .expect("parse event file");
        connection
            .execute(
                "INSERT INTO event_log VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    event["sequence"].as_u64().expect("event sequence"),
                    event["machine"].as_str().expect("event machine"),
                    event["previous_sha256"]
                        .as_str()
                        .expect("previous event digest"),
                    event["recorded_at"].as_str().expect("event timestamp"),
                    event["kind"].as_str().expect("event kind"),
                    crate::canonical::canonical_json(&event["payload"])
                        .expect("canonical event payload"),
                    event["sha256"].as_str().expect("event digest"),
                ],
            )
            .expect("insert legacy event");
    }
    for (slot, record) in &replayed.active_records {
        connection
            .execute(
                "INSERT INTO active_records VALUES (?1, ?2)",
                params![
                    slot,
                    crate::canonical::canonical_json(record).expect("canonical active record"),
                ],
            )
            .expect("insert legacy active row");
    }
    for record in &replayed.archive_records {
        connection
            .execute(
                "INSERT INTO archive_records VALUES (?1, ?2, ?3)",
                params![
                    record["archive_id"].as_str().expect("archive id"),
                    record["slot"].as_str().expect("archive slot"),
                    crate::canonical::canonical_json(record).expect("canonical archive record"),
                ],
            )
            .expect("insert legacy archive row");
    }
}

#[test]
fn schema_one_index_rows_remain_readable_as_unknown() {
    let scratch = Scratch::new();
    let events = scratch.events();
    one_event_log(&events);
    let index = scratch.0.join("schema-one.sqlite");
    write_schema_one_index(&events, &index);

    assert_eq!(
        read_index(&index)
            .expect("read schema-one index")
            .active_count,
        1
    );
    let decision = read_decision(&index, "slot-a").expect("explain schema-one row");
    assert_eq!(decision.verdict, Verdict::Unknown);
    assert_eq!(decision.reason_codes, ["LEGACY_INDEX_SCHEMA"]);

    rebuild_index(&events, &index).expect("migrate schema-one index by replay");
    let migrated = read_decision(&index, "slot-a").expect("read migrated row");
    assert_eq!(migrated.verdict, Verdict::Unknown);
    assert_eq!(migrated.reason_codes, ["POLICY_INPUTS_MISSING"]);
}

#[test]
fn synthetic_python_audit_differential_keeps_legacy_rows_fail_closed() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = active_record("slot-a", 1);
    import_log(&events, vec![record.clone()], vec![]);
    let python = python_synthetic_audit_verdict(&events);
    assert!(
        python.status.success(),
        "Python synthetic audit failed: {}",
        String::from_utf8_lossy(&python.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&python.stdout).trim(), "DELETABLE");

    let (config, evidence) =
        write_policy_inputs(&scratch, &[record], "2026-09-22T09:40:01+00:00", |_| {});
    let index = scratch.0.join("python-differential.sqlite");
    rebuild_policy_index(&events, &index, &config, &evidence).expect("build shadow index");
    let rust = read_decision(&index, "slot-a").expect("read shadow decision");
    assert_eq!(rust.verdict, Verdict::Unknown);
    assert_eq!(
        rust.reason_codes,
        [
            "TASKGRAPH_CLAIM_UNVERIFIED",
            "LEGACY_SCOPE_IDENTITY_MISSING"
        ]
    );
}

#[test]
fn pressure_plan_scales_to_250_noop_rows_and_preserves_every_decision() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let records = (0..250_u64)
        .map(|index| scoped_active_record(&format!("slot-{index:03}"), index + 1))
        .collect::<Vec<_>>();
    import_log(&events, records.clone(), vec![]);
    let (config, evidence) =
        write_policy_inputs(&scratch, &records, "2026-09-22T09:40:01+00:00", |_| {});
    let index = scratch.0.join("scale.sqlite");
    let before = fs::read_dir(&events)
        .expect("list event files")
        .map(|entry| {
            let entry = entry.expect("read event entry");
            (
                entry.file_name(),
                fs::read(entry.path()).expect("read event bytes"),
            )
        })
        .collect::<Vec<_>>();
    let started = std::time::Instant::now();
    rebuild_policy_index(&events, &index, &config, &evidence).expect("index 250 rows");
    let plan = read_pressure_plan(&index, 0, None).expect("plan 250 rows");
    assert!(plan.eligible.is_empty() && plan.deferred_eligible.is_empty());
    assert!(plan.blocked.is_empty());
    assert_eq!(plan.unknown.len(), 250);
    assert!(plan.unknown.iter().all(|decision| {
        decision
            .reason_codes
            .iter()
            .any(|reason| reason == "TASKGRAPH_CLAIM_UNVERIFIED")
            && decision
                .reason_codes
                .iter()
                .any(|reason| reason == "TASK_SCOPE_RUNTIME_UNVERIFIED")
    }));
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    let after = fs::read_dir(&events)
        .expect("re-list event files")
        .map(|entry| {
            let entry = entry.expect("read event entry");
            (
                entry.file_name(),
                fs::read(entry.path()).expect("re-read event bytes"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(before, after, "shadow evaluation changed source events");
}

#[test]
fn pressure_plan_preserves_subsecond_order_limits_targets_and_large_totals() {
    let scratch = Scratch::new();
    let mut later = policy_decision(
        &scratch,
        scoped_active_record("slot-a", 1),
        "2026-09-22T09:40:01+00:00",
        |_| {},
    );
    later.slot = "later".to_owned();
    later.heartbeat_at = Some("2026-09-22T09:30:00.000000002+00:00".to_owned());
    later.reclaimable_bytes = u64::MAX;
    // Exercise the generic future planner independently of the current
    // rollout gates, which deliberately prevent policy evaluation from
    // producing an actionable candidate.
    later.verdict = Verdict::Eligible;
    later.reason_codes.clear();
    let mut earlier = later.clone();
    earlier.slot = "earlier".to_owned();
    earlier.heartbeat_at = Some("2026-09-22T09:30:00.000000001+00:00".to_owned());

    let bounded = crate::plan::build(vec![later.clone(), earlier.clone()], 1, 1);
    assert_eq!(bounded.eligible[0].slot, "earlier");
    assert_eq!(bounded.deferred_eligible[0].slot, "later");
    assert!(bounded.target_met);

    let unbounded = crate::plan::build(vec![later, earlier], 0, 2);
    assert_eq!(
        unbounded.selected_reclaimable_bytes,
        u128::from(u64::MAX) * 2
    );
    assert_eq!(
        serde_json::to_value(&unbounded).expect("encode large plan")["selected_reclaimable_bytes"],
        Value::String((u128::from(u64::MAX) * 2).to_string())
    );

    let limited = Scratch::new();
    let events = limited.events();
    let records = vec![
        scoped_active_record("slot-a", 1),
        scoped_active_record("slot-b", 2),
    ];
    import_log(&events, records.clone(), vec![]);
    let (config, evidence) =
        write_policy_inputs(&limited, &records, "2026-09-22T09:40:01+00:00", |_| {});
    rewrite_json(&config, |value| value["max_plan_slots"] = json!(1));
    let index = limited.0.join("limited.sqlite");
    rebuild_policy_index(&events, &index, &config, &evidence).expect("build limited plan index");
    let plan = read_pressure_plan(&index, 0, Some(20)).expect("read configured-limit plan");
    assert!(plan.eligible.is_empty() && plan.deferred_eligible.is_empty());
    assert_eq!(plan.unknown.len(), 2);
}

#[test]
fn cli_surface_contains_no_executor_or_mutation_subcommand() {
    use clap::CommandFactory as _;

    let command = crate::cli::Cli::command();
    let names = command
        .get_subcommands()
        .map(|subcommand| subcommand.get_name())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        names,
        BTreeSet::from(["explain", "plan", "rebuild", "status"])
    );
    for subcommand in command.get_subcommands() {
        for argument in subcommand.get_arguments() {
            let id = argument.get_id().as_str();
            assert!(
                !matches!(id, "apply" | "delete" | "remove" | "execute" | "mutate"),
                "forbidden mutation argument on {}: {id}",
                subcommand.get_name()
            );
        }
    }
}

const ROLLOUT_GATES: [&str; 2] = [
    "TASK_SCOPE_RUNTIME_UNVERIFIED",
    "TASKGRAPH_CLAIM_UNVERIFIED",
];

fn with_rollout_gates(reasons: &[&str]) -> Vec<String> {
    ROLLOUT_GATES
        .iter()
        .chain(reasons)
        .map(|reason| (*reason).to_owned())
        .collect()
}

type DecisionMutation = fn(&mut crate::policy::PolicyDecision);
type EvidenceMutation = fn(&mut Value);

fn evaluate_single_policy(
    record: Value,
    census_started_at: &str,
    observed_at: &str,
    evaluated_at: &str,
) -> Result<crate::policy::PolicyDecision, crate::ObserverError> {
    let scratch = Scratch::new();
    let events = scratch.events();
    import_log(&events, vec![record.clone()], vec![]);
    let (config, evidence) = write_policy_inputs(&scratch, &[record], observed_at, |value| {
        value["census_started_at"] = Value::String(census_started_at.to_owned());
    });
    evaluate_policy_at(&events, &config, &evidence, evaluated_at)
        .map(|mut decisions| decisions.remove(0))
}

#[test]
fn heartbeat_after_the_lifecycle_instant_keeps_liveness_blockers(
) -> Result<(), crate::ObserverError> {
    // The imported event tip is recorded at 09:35:01 and the fixture skew is
    // 60 s, so a census may begin as early as 09:34:01.
    let mut record = scoped_active_record("slot-a", 1);
    record["heartbeat_at"] = Value::String("2026-09-22T09:35:01+00:00".to_owned());

    let at_skew = evaluate_single_policy(
        record.clone(),
        "2026-09-22T09:34:01+00:00",
        "2026-09-22T09:34:01+00:00",
        "2026-09-22T09:34:01+00:00",
    )?;
    assert_eq!(at_skew.verdict, Verdict::Blocked);
    assert_eq!(
        at_skew.reason_codes,
        with_rollout_gates(&["HEARTBEAT_TTL_ACTIVE", "MINIMUM_STALE_AGE_ACTIVE"])
    );

    let error = evaluate_single_policy(
        record.clone(),
        "2026-09-22T09:34:00.999999999+00:00",
        "2026-09-22T09:34:00.999999999+00:00",
        "2026-09-22T09:34:00.999999999+00:00",
    )
    .expect_err("a census beyond the event-tip skew is refused")
    .to_string();
    assert!(error.contains("event tip"), "{error}");

    // Evaluation may precede the census by up to the skew as well; a heartbeat
    // more than one skew after the lifecycle instant is reported, but it is
    // still younger than every TTL.
    let beyond_skew = evaluate_single_policy(
        record,
        "2026-09-22T09:34:01+00:00",
        "2026-09-22T09:34:01+00:00",
        "2026-09-22T09:34:00.999999999+00:00",
    )?;
    assert_eq!(beyond_skew.verdict, Verdict::Blocked);
    assert_eq!(
        beyond_skew.reason_codes,
        with_rollout_gates(&[
            "CLOCK_BEFORE_HEARTBEAT",
            "HEARTBEAT_TTL_ACTIVE",
            "MINIMUM_STALE_AGE_ACTIVE",
        ])
    );
    Ok(())
}

#[test]
fn missing_slot_evidence_keeps_heartbeat_blockers() {
    let scratch = Scratch::new();
    let decision = policy_decision(
        &scratch,
        scoped_active_record("slot-a", 1),
        "2026-09-22T09:40:00+00:00",
        |value| value["slots"] = json!([]),
    );
    assert_eq!(decision.verdict, Verdict::Blocked);
    assert_eq!(
        decision.reason_codes,
        with_rollout_gates(&["HEARTBEAT_TTL_ACTIVE", "EVIDENCE_MISSING"])
    );
}

#[test]
fn schema_valid_non_rfc3339_heartbeats_keep_their_ttl_blockers() -> Result<(), crate::ObserverError>
{
    for (heartbeat, boundary, after) in [
        (
            "20260922T093000Z",
            "2026-09-22T09:40:00+00:00",
            "2026-09-22T09:40:00.000000001+00:00",
        ),
        (
            "2026-W39-2T0930Z",
            "2026-09-22T09:40:00+00:00",
            "2026-09-22T09:40:00.000000001+00:00",
        ),
        (
            "2026-09-22T11:30:00,5+02:00",
            "2026-09-22T09:40:00.5+00:00",
            "2026-09-22T09:40:00.500000001+00:00",
        ),
    ] {
        assert!(parse_timestamp(heartbeat, "fixture heartbeat").is_ok());
        assert!(chrono::DateTime::parse_from_rfc3339(heartbeat).is_err());
        let mut record = scoped_active_record("slot-a", 1);
        record["heartbeat_at"] = Value::String(heartbeat.to_owned());
        let at_ttl = evaluate_single_policy(record.clone(), boundary, boundary, boundary)?;
        assert_eq!(at_ttl.verdict, Verdict::Blocked, "{heartbeat}");
        assert_eq!(
            at_ttl.reason_codes,
            with_rollout_gates(&["HEARTBEAT_TTL_ACTIVE"]),
            "{heartbeat}"
        );
        let expired = evaluate_single_policy(record, after, after, after)?;
        assert_rollout_gated(&expired);
        assert_eq!(expired.reason_codes, with_rollout_gates(&[]), "{heartbeat}");
    }
    Ok(())
}

#[test]
fn policy_input_names_use_the_replay_name_grammar() {
    // Evidence names must match names replay has already validated, so the
    // policy inputs use the same leading-alphanumeric grammar as replay.
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = scoped_active_record("slot-a", 1);
    import_log(&events, vec![record.clone()], vec![]);
    let (config, evidence) = write_policy_inputs(
        &scratch,
        std::slice::from_ref(&record),
        "2026-09-22T09:40:01+00:00",
        |_| {},
    );
    ShadowConfig::load(&config).expect("baseline configuration loads");
    EvidenceBundle::load(&evidence).expect("baseline evidence loads");
    let original_config = fs::read(&config).expect("read configuration");
    let original_evidence = fs::read(&evidence).expect("read evidence");
    for prefix in [".", "_", "-"] {
        fs::write(&config, &original_config).expect("restore configuration");
        rewrite_json(&config, |value| {
            value["machine"] = Value::String(format!("{prefix}node-a"));
        });
        let error = ShadowConfig::load(&config)
            .err()
            .unwrap_or_else(|| panic!("configuration machine {prefix:?} must be refused"))
            .to_string();
        assert!(error.contains("is not a valid name"), "{error}");
        fs::write(&config, &original_config).expect("restore configuration");

        type EvidenceMutation = fn(&mut Value, &str);
        let mutations: [(&str, EvidenceMutation); 3] = [
            ("machine", |value, name| {
                value["machine"] = Value::String(name.to_owned());
            }),
            ("slot", |value, name| {
                value["slots"][0]["slot"] = Value::String(name.to_owned());
            }),
            ("checkout", |value, name| {
                value["slots"][0]["checkouts"][0]["name"] = Value::String(name.to_owned());
            }),
        ];
        for (field, mutate) in mutations {
            fs::write(&evidence, &original_evidence).expect("restore evidence");
            rewrite_json(&evidence, |value| mutate(value, &format!("{prefix}name")));
            let error = EvidenceBundle::load(&evidence)
                .err()
                .unwrap_or_else(|| panic!("evidence {field} {prefix:?} must be refused"))
                .to_string();
            assert!(error.contains("is not a valid name"), "{field}: {error}");
        }
    }
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[test]
fn policy_inputs_refuse_unknown_keys_schemas_variants_and_verifications() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = scoped_active_record("slot-a", 1);
    import_log(&events, vec![record.clone()], vec![]);
    let (config, evidence) = write_policy_inputs(
        &scratch,
        std::slice::from_ref(&record),
        "2026-09-22T09:40:01+00:00",
        |_| {},
    );
    ShadowConfig::load(&config).expect("baseline configuration loads");
    EvidenceBundle::load(&evidence).expect("baseline evidence loads");
    let original_config = fs::read(&config).expect("read configuration");
    let original_evidence = fs::read(&evidence).expect("read evidence");

    type Mutation = fn(&mut Value);
    let config_cases: [(&str, Mutation, &str); 2] = [
        (
            "unknown configuration key",
            |value| value["unexpected"] = json!(1),
            "unknown field `unexpected`",
        ),
        (
            "configuration schema 2",
            |value| value["schema"] = json!(2),
            "unsupported shadow policy configuration schema 2",
        ),
    ];
    for (case, mutate, expected) in config_cases {
        fs::write(&config, &original_config).expect("restore configuration");
        rewrite_json(&config, mutate);
        let error = ShadowConfig::load(&config)
            .err()
            .unwrap_or_else(|| panic!("{case} must be refused"));
        let error = error_chain(&error);
        assert!(error.contains(expected), "{case}: {error}");
    }

    let evidence_cases: [(&str, Mutation, &str); 9] = [
        (
            "unknown bundle key",
            |value| value["unexpected"] = json!(1),
            "unknown field `unexpected`",
        ),
        (
            "unknown slot key",
            |value| value["slots"][0]["unexpected"] = json!(1),
            "unknown field `unexpected`",
        ),
        (
            "unknown scope key",
            |value| value["slots"][0]["scope"]["unexpected"] = json!(1),
            "unknown field `unexpected`",
        ),
        (
            "unknown scope identity key",
            |value| value["slots"][0]["scope"]["recorded"]["unexpected"] = json!(1),
            "unknown field `unexpected`",
        ),
        (
            "unknown checkout key",
            |value| value["slots"][0]["checkouts"][0]["unexpected"] = json!(1),
            "unknown field `unexpected`",
        ),
        (
            "unknown process-use variant",
            |value| value["slots"][0]["process_use"] = json!("maybe"),
            "unknown variant `maybe`",
        ),
        (
            "unknown journal variant",
            |value| value["slots"][0]["journal"] = json!("maybe"),
            "unknown variant `maybe`",
        ),
        (
            "evidence schema 2",
            |value| value["schema"] = json!(2),
            "unsupported shadow policy evidence schema 2",
        ),
        (
            "unsupported scope verification",
            |value| {
                value["slots"][0]["scope"]["recorded"]["verification"] =
                    json!("systemd-runtime-invocation-symlink-v2");
            },
            "scope.verification is not a supported writer verification",
        ),
    ];
    for (case, mutate, expected) in evidence_cases {
        fs::write(&evidence, &original_evidence).expect("restore evidence");
        rewrite_json(&evidence, mutate);
        let error = EvidenceBundle::load(&evidence)
            .err()
            .unwrap_or_else(|| panic!("{case} must be refused"));
        let error = error_chain(&error);
        assert!(error.contains(expected), "{case}: {error}");
    }
}

#[test]
fn heartbeat_fields_that_cannot_be_aged_never_drop_blockers() -> Result<(), crate::ObserverError> {
    let scratch = Scratch::new();
    let base = policy_decision(
        &scratch,
        scoped_active_record("slot-a", 1),
        "2026-09-22T09:40:00+00:00",
        |_| {},
    );
    let lifecycle_at = chrono::DateTime::parse_from_rfc3339("2026-09-22T09:40:00+00:00")
        .expect("fixture lifecycle instant");
    let config = ShadowConfig::load(&scratch.0.join("policy.json"))?.value;
    let mut stale_floor = config.clone();
    stale_floor.minimum_stale_seconds = 900;
    let cases: [(&ShadowConfig, DecisionMutation, Verdict, &[&str]); 4] = [
        (
            &config,
            |decision| decision.heartbeat_at = None,
            Verdict::Blocked,
            &["ACTIVE_HEARTBEAT_MISSING"],
        ),
        (
            &config,
            |decision| decision.heartbeat_at = Some("not a timestamp".to_owned()),
            Verdict::Blocked,
            &["HEARTBEAT_TIMESTAMP_UNSUPPORTED"],
        ),
        (
            &config,
            |decision| decision.heartbeat_ttl_seconds = None,
            Verdict::Unknown,
            &["ACTIVE_HEARTBEAT_TTL_MISSING"],
        ),
        (
            &stale_floor,
            |decision| decision.heartbeat_ttl_seconds = None,
            Verdict::Blocked,
            &["ACTIVE_HEARTBEAT_TTL_MISSING", "MINIMUM_STALE_AGE_ACTIVE"],
        ),
    ];
    for (config, mutate, verdict, reasons) in cases {
        let mut decision = base.clone();
        decision.verdict = Verdict::Eligible;
        decision.reason_codes.clear();
        mutate(&mut decision);
        policy::evaluate_time(&mut decision, config, lifecycle_at)?;
        assert_eq!(decision.verdict, verdict, "{reasons:?}");
        assert_eq!(decision.reason_codes, reasons, "{reasons:?}");
    }
    Ok(())
}

#[test]
fn each_policy_reason_code_is_isolated_by_one_evidence_field() {
    let baseline_scratch = Scratch::new();
    let baseline = policy_decision(
        &baseline_scratch,
        scoped_active_record("slot-a", 1),
        "2026-09-22T09:40:01+00:00",
        |_| {},
    );
    assert_eq!(baseline.verdict, Verdict::Unknown);
    assert_eq!(baseline.reason_codes, with_rollout_gates(&[]));

    // The baseline scope is `dead` with an `absent` leader, so the leader
    // cases below cannot be satisfied by the scope-liveness blocker.
    let cases: [(&str, Verdict, EvidenceMutation); 9] = [
        ("EVIDENCE_MISSING", Verdict::Unknown, |value| {
            value["slots"] = json!([]);
        }),
        ("SCOPE_EVIDENCE_MISSING", Verdict::Unknown, |value| {
            value["slots"][0]["scope"] = Value::Null;
        }),
        ("TASK_LEADER_LIVE", Verdict::Blocked, |value| {
            value["slots"][0]["scope"]["leader_state"] = json!("same");
        }),
        ("TASK_LEADER_STATE_UNKNOWN", Verdict::Unknown, |value| {
            value["slots"][0]["scope"]["leader_state"] = json!("unknown");
        }),
        ("JOURNAL_STATE_UNKNOWN", Verdict::Unknown, |value| {
            value["slots"][0]["journal"] = json!("unknown");
        }),
        ("CHECKOUT_NOT_DIRECTORY", Verdict::Blocked, |value| {
            value["slots"][0]["checkouts"][0]["directory"] = json!(false);
        }),
        ("CHECKOUT_PATH_DRIFT", Verdict::Unknown, |value| {
            value["slots"][0]["checkouts"][0]["path"] = json!("worktrees/slots/slot-a/other");
        }),
        ("CHECKOUT_EVIDENCE_MISSING", Verdict::Unknown, |value| {
            value["slots"][0]["checkouts"][0]["name"] = json!("other");
        }),
        ("CHECKOUT_EVIDENCE_SET_DRIFT", Verdict::Unknown, |value| {
            let extra = json!({
                "name": "extra",
                "path": "worktrees/slots/slot-a/extra",
                "device": 11,
                "inode": 22,
                "mount_id": 33,
                "directory": true,
                "symlink_free": true,
                "mount_stable": true,
            });
            value["slots"][0]["checkouts"]
                .as_array_mut()
                .expect("fixture checkouts")
                .push(extra);
        }),
    ];
    for (reason, verdict, mutate) in cases {
        let scratch = Scratch::new();
        let decision = policy_decision(
            &scratch,
            scoped_active_record("slot-a", 1),
            "2026-09-22T09:40:01+00:00",
            mutate,
        );
        assert_eq!(decision.verdict, verdict, "{reason}");
        assert_eq!(
            decision.reason_codes,
            with_rollout_gates(&[reason]),
            "{reason}"
        );
    }
}

fn append_retirement_attempt(events: &Path, sequence: u64, tip: &str, generation: u64) -> String {
    append_event(
        events,
        sequence,
        tip,
        "retirement-attempted",
        json!({
            "slot": "slot-a",
            "generation": generation,
            "sha256": "a".repeat(64),
            "handoff_read_sequence": 1,
            "reason": "bounded queue attempt",
        }),
    )
}

#[test]
fn pending_generation_conflicts_are_reported_for_active_and_pending_only_slots(
) -> Result<(), crate::ObserverError> {
    // A generation-1 retirement marker survives an in-place ACTIVE replacement
    // by generation 2; the baseline records the marker after the replacement.
    for conflicting in [false, true] {
        let scratch = Scratch::new();
        let events = scratch.events();
        let first = scoped_active_record("slot-a", 1);
        let second = scoped_active_record("slot-a", 2);
        let mut tip = import_log(&events, vec![first.clone()], vec![]);
        let mut sequence = 2;
        if conflicting {
            tip = append_retirement_attempt(&events, sequence, &tip, 1);
            sequence += 1;
        }
        tip = append_event(
            &events,
            sequence,
            &tip,
            "active-state-recorded",
            json!({
                "action": "slot-updated",
                "slot": "slot-a",
                "previous_revision": 0,
                "revision": 1,
                "previous_record_sha256": canonical_sha256(&first).expect("active digest"),
                "record": second,
                "evidence": {},
            }),
        );
        if !conflicting {
            append_retirement_attempt(&events, sequence + 1, &tip, 2);
        }
        let replayed = replay_stream(&events, None, |_| Ok(()))?;
        let (config, evidence) = write_policy_inputs(
            &scratch,
            &[replayed.active_records["slot-a"].clone()],
            "2026-09-22T09:40:01+00:00",
            |_| {},
        );
        let decision =
            evaluate_policy_at(&events, &config, &evidence, "2026-09-22T09:40:01+00:00")?.remove(0);
        assert_eq!(decision.verdict, Verdict::Blocked);
        // The pending marker also disagrees with the fixture's `absent` journal.
        let expected: &[&str] = if conflicting {
            &[
                "PENDING_GENERATION_CONFLICT",
                "RETIREMENT_PENDING",
                "JOURNAL_EVENT_EVIDENCE_CONFLICT",
            ]
        } else {
            &["RETIREMENT_PENDING", "JOURNAL_EVENT_EVIDENCE_CONFLICT"]
        };
        assert_eq!(decision.reason_codes, with_rollout_gates(expected));
    }

    // Without an ACTIVE row, markers that disagree about the generation leave
    // the pending-only decision's generation null and report the conflict.
    for conflicting in [false, true] {
        let scratch = Scratch::new();
        let events = scratch.events();
        let record = scoped_active_record("slot-a", 1);
        let mut tip = import_log(&events, vec![record.clone()], vec![]);
        let mut sequence = 2;
        if conflicting {
            tip = append_retirement_attempt(&events, sequence, &tip, 1);
            sequence += 1;
        }
        tip = append_event(
            &events,
            sequence,
            &tip,
            "active-state-recorded",
            json!({
                "action": "slot-removed",
                "slot": "slot-a",
                "previous_revision": 0,
                "revision": 1,
                "previous_record_sha256": canonical_sha256(&record).expect("active digest"),
                "record": null,
                "evidence": {},
            }),
        );
        append_event(
            &events,
            sequence + 1,
            &tip,
            "operation-progress-recorded",
            json!({
                "slot": "slot-a",
                "operation": "create",
                "journal": {"schema": 2, "kind": "create", "machine": "node-a", "slot": "slot-a"},
            }),
        );
        let (config, evidence) =
            write_policy_inputs(&scratch, &[], "2026-09-22T09:40:01+00:00", |_| {});
        let decision =
            evaluate_policy_at(&events, &config, &evidence, "2026-09-22T09:40:01+00:00")?.remove(0);
        let expected: &[&str] = if conflicting {
            &[
                "ACTIVE_RECORD_MISSING",
                "PENDING_GENERATION_CONFLICT",
                "OPERATION_JOURNAL_PENDING",
                "RETIREMENT_PENDING",
            ]
        } else {
            &["ACTIVE_RECORD_MISSING", "OPERATION_JOURNAL_PENDING"]
        };
        assert_eq!(decision.verdict, Verdict::Blocked);
        assert_eq!(decision.generation, None);
        assert_eq!(decision.reason_codes, expected);

        let plain_index = scratch.0.join("pending-generation-plain.sqlite");
        rebuild_index(&events, &plain_index)?;
        let plain = read_decision(&plain_index, "slot-a")?;
        assert_eq!(plain.verdict, Verdict::Blocked);
        assert_eq!(plain.generation, None);
        assert_eq!(
            plain.reason_codes,
            std::iter::once("POLICY_INPUTS_MISSING")
                .chain(expected.iter().copied())
                .collect::<Vec<_>>()
        );
    }
    Ok(())
}

fn singleton_finish_progress(events: &Path, sequence: u64, tip: &str, slot: &str) -> String {
    append_event(
        events,
        sequence,
        tip,
        "operation-progress-recorded",
        json!({
            "slot": slot,
            "operation": "finish",
            "journal_path": "ACTIVE.node-a.journal",
            "journal": {"schema": 2, "kind": "finish", "machine": "node-a", "slot": slot},
        }),
    )
}

#[test]
fn journal_marker_divergences_from_python_are_fail_closed() {
    // Python overwrites a pending marker when the reused singleton journal
    // path names another operation identity; Rust refuses the history.
    let reused = Scratch::new();
    let reused_events = reused.events();
    let mut tip = import_log(
        &reused_events,
        vec![active_record("slot-a", 1), active_record("slot-b", 1)],
        vec![],
    );
    tip = singleton_finish_progress(&reused_events, 2, &tip, "slot-a");
    singleton_finish_progress(&reused_events, 3, &tip, "slot-b");
    assert_eq!(
        python_pending_journals(&reused_events),
        json!({"ACTIVE.node-a.journal": ["slot-b", "finish"]})
    );
    let error = replay(&reused_events)
        .expect_err("a reused pending journal path must fail closed")
        .to_string();
    assert!(error.contains("reuses a pending journal path"), "{error}");

    // Python ignores a completion without a pending marker; Rust refuses it
    // unless a matching recovery attempt or identical completion exists.
    let naked = Scratch::new();
    let naked_events = naked.events();
    let first = import_log(&naked_events, vec![active_record("slot-a", 1)], vec![]);
    append_event(
        &naked_events,
        2,
        &first,
        "operation-completed",
        json!({
            "slot": "slot-a",
            "operation": "finish",
            "journal_path": "ACTIVE.node-a.journal",
        }),
    );
    assert_eq!(python_pending_journals(&naked_events), json!({}));
    let error = replay(&naked_events)
        .expect_err("an unmatched completion must fail closed")
        .to_string();
    assert!(error.contains("no pending progress or recovery"), "{error}");
}

#[test]
fn recovery_of_a_completed_scoped_journal_leaves_no_phantom_marker() {
    // `operation-completed` is appended before the journal file is unlinked.
    // A crash between the two leaves a completed FINISH journal on disk that
    // `recover` loads again; its recovery must not invent a singleton marker.
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = scoped_active_record("slot-a", 1);
    let finish_path = "FINISH.6.node-a.6.slot-a.journal";
    let mut tip = import_log(&events, vec![record.clone()], vec![]);
    tip = append_event(
        &events,
        2,
        &tip,
        "operation-progress-recorded",
        json!({
            "slot": "slot-a",
            "operation": "finish",
            "journal_path": finish_path,
            "journal": {
                "schema": 2,
                "kind": "finish",
                "machine": "node-a",
                "slot": "slot-a",
                "phase": "prepared",
                "record": record.clone(),
            },
        }),
    );
    tip = append_event(
        &events,
        3,
        &tip,
        "archive-state-recorded",
        json!({
            "action": "slot-removed",
            "slot": "slot-a",
            "previous_revision": 0,
            "revision": 1,
            "record": archive_record("slot-a", 1),
            "evidence": {},
        }),
    );
    tip = append_event(
        &events,
        4,
        &tip,
        "active-state-recorded",
        json!({
            "action": "slot-removed",
            "slot": "slot-a",
            "previous_revision": 0,
            "revision": 1,
            "previous_record_sha256": canonical_sha256(&record).expect("active digest"),
            "record": null,
            "evidence": {},
        }),
    );
    let completion = json!({
        "slot": "slot-a",
        "operation": "finish",
        "journal_path": finish_path,
    });
    tip = append_event(&events, 5, &tip, "operation-completed", completion.clone());
    tip = append_event(
        &events,
        6,
        &tip,
        "recovery-started",
        json!({
            "slot": "slot-a",
            "operation": "finish",
            "actor": identity(12),
            "runner": identity(13),
            "handoff_writer": null,
            "coordinator_authorized": true,
        }),
    );
    let recovering = replay_stream(&events, None, |_| Ok(())).expect("replay recovery");
    assert!(
        recovering.pending_operations.is_empty(),
        "{:?}",
        recovering.pending_operations
    );
    append_event(&events, 7, &tip, "operation-completed", completion);
    let recovered = replay_stream(&events, None, |_| Ok(())).expect("replay recovered finish");
    assert!(
        recovered.pending_operations.is_empty(),
        "{:?}",
        recovered.pending_operations
    );
    assert_eq!(python_pending_journals(&events), json!({}));
}

#[test]
fn recovery_binds_the_most_recent_completed_journal_path() {
    // Recovery of an operation with no pending journal reloads the journal
    // that completed last. Binding the path that merely sorts first would
    // leave a phantom legacy-journal marker once another slot reuses it.
    let scratch = Scratch::new();
    let events = scratch.events();
    let slot_a = active_record("slot-a", 1);
    let slot_b = active_record("slot-b", 1);
    let finish_path = "FINISH.6.node-a.6.slot-a.journal";
    let singleton = "ACTIVE.node-a.journal";
    let completion =
        |slot: &str, path: &str| json!({"slot": slot, "operation": "finish", "journal_path": path});
    let mut tip = import_log(&events, vec![slot_a.clone(), slot_b.clone()], vec![]);
    // 1. slot-a finishes through the singleton path and rolls back.
    tip = singleton_finish_progress(&events, 2, &tip, "slot-a");
    tip = append_event(
        &events,
        3,
        &tip,
        "operation-completed",
        completion("slot-a", singleton),
    );
    // 2. A scoped finish advances through its archive and remove phases and
    //    completes while slot-a is retained.
    for (sequence, phase) in [(4, "prepared"), (5, "archive"), (6, "remove")] {
        tip = append_event(
            &events,
            sequence,
            &tip,
            "operation-progress-recorded",
            json!({
                "slot": "slot-a",
                "operation": "finish",
                "journal_path": finish_path,
                "journal": {
                    "schema": 2,
                    "kind": "finish",
                    "machine": "node-a",
                    "slot": "slot-a",
                    "phase": phase,
                },
            }),
        );
    }
    tip = append_event(
        &events,
        7,
        &tip,
        "operation-completed",
        completion("slot-a", finish_path),
    );
    // 3. Recovery starts while slot-a is still ACTIVE.
    tip = append_event(
        &events,
        8,
        &tip,
        "recovery-started",
        recovery_started_payload("slot-a", "finish"),
    );
    // 4. slot-b reuses the singleton path and completes.
    tip = singleton_finish_progress(&events, 9, &tip, "slot-b");
    tip = append_event(
        &events,
        10,
        &tip,
        "operation-completed",
        completion("slot-b", singleton),
    );
    // 5. slot-a is archived, removed, and its finish completes.
    tip = append_event(
        &events,
        11,
        &tip,
        "archive-state-recorded",
        json!({
            "action": "slot-removed",
            "slot": "slot-a",
            "previous_revision": 0,
            "revision": 1,
            "record": archive_record("slot-a", 1),
            "evidence": {},
        }),
    );
    tip = append_event(
        &events,
        12,
        &tip,
        "active-state-recorded",
        json!({
            "action": "slot-removed",
            "slot": "slot-a",
            "previous_revision": 0,
            "revision": 1,
            "previous_record_sha256": canonical_sha256(&slot_a).expect("active digest"),
            "record": null,
            "evidence": {},
        }),
    );
    append_event(
        &events,
        13,
        &tip,
        "operation-completed",
        completion("slot-a", finish_path),
    );
    assert_eq!(python_pending_journals(&events), json!({}));
    let replayed = replay_stream(&events, None, |_| Ok(())).expect("replay history");
    assert!(
        replayed.pending_operations.is_empty(),
        "{:?}",
        replayed.pending_operations
    );
}

/// Where a create or import recovery writes the slot's ACTIVE row relative to
/// the `recovery-started` event, or whether it writes none at all.
#[derive(Clone, Copy, Debug)]
enum CreateImportOutcome {
    /// The row was durable before recovery began (`_recover_create` and
    /// `_recover_import_existing` clear an already-durable journal).
    RowDurable,
    /// Recovery publishes the row, then clears the journal.
    RowPublishedByRecovery,
    /// `--abort-create` / `--abort-import`, or a writer that completed the
    /// journal without publishing: no row exists at completion.
    Aborted,
}

fn recovery_started_payload(slot: &str, operation: &str) -> Value {
    json!({
        "slot": slot,
        "operation": operation,
        "actor": identity(12),
        "runner": identity(13),
        "handoff_writer": null,
        "coordinator_authorized": true,
    })
}

/// Replays one interrupted create or import through a recovery that completes
/// its journal, returning the event directory's scratch owner and the record.
fn create_import_recovery_log(
    operation: &str,
    journal_path: &str,
    outcome: CreateImportOutcome,
) -> (Scratch, PathBuf, Value) {
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = if operation == "import-existing" {
        imported_active_record("slot-a", "agent-slot-a", 1)
    } else {
        active_record("slot-a", 1)
    };
    let publish = |sequence: u64, tip: &str, action: &str| {
        append_event(
            &events,
            sequence,
            tip,
            "active-state-recorded",
            json!({
                "action": action,
                "slot": "slot-a",
                "previous_revision": 0,
                "revision": 1,
                "previous_record_sha256": null,
                "record": record.clone(),
                "evidence": {},
            }),
        )
    };
    let (published, recovered) = if operation == "import-existing" {
        ("slot-imported", "slot-imported-by-recovery")
    } else {
        ("slot-created", "slot-created-by-recovery")
    };
    let mut sequence = 1;
    let mut tip = import_log(&events, vec![], vec![]);
    sequence += 1;
    tip = append_event(
        &events,
        sequence,
        &tip,
        "operation-progress-recorded",
        json!({
            "slot": "slot-a",
            "operation": operation,
            "journal_path": journal_path,
            "journal": attempt_journal(operation, &record),
        }),
    );
    if matches!(outcome, CreateImportOutcome::RowDurable) {
        sequence += 1;
        tip = publish(sequence, &tip, published);
    }
    sequence += 1;
    tip = append_event(
        &events,
        sequence,
        &tip,
        "recovery-started",
        recovery_started_payload("slot-a", operation),
    );
    if matches!(outcome, CreateImportOutcome::RowPublishedByRecovery) {
        sequence += 1;
        tip = publish(sequence, &tip, recovered);
    }
    sequence += 1;
    append_event(
        &events,
        sequence,
        &tip,
        "operation-completed",
        json!({"slot": "slot-a", "operation": operation, "journal_path": journal_path}),
    );
    (scratch, events, record)
}

#[test]
fn completed_create_and_import_recoveries_close_their_marker_only_with_a_row() {
    // A create or import completion that leaves an ACTIVE row hands the slot's
    // storage to that row, so it is terminal for the recovery attempt too.
    // Without a row the completion proves nothing about storage: an older
    // writer completed a create journal while leaving its worktree behind.
    let create_path = "CREATE.6.node-a.6.slot-a.journal";
    let singleton = "ACTIVE.node-a.journal";
    for (operation, journal_path) in [
        ("create", create_path),
        ("create", singleton),
        ("import-existing", singleton),
    ] {
        for outcome in [
            CreateImportOutcome::RowDurable,
            CreateImportOutcome::RowPublishedByRecovery,
            CreateImportOutcome::Aborted,
        ] {
            let case = format!("{operation} at {journal_path} ({outcome:?})");
            let (scratch, events, record) =
                create_import_recovery_log(operation, journal_path, outcome);
            assert_eq!(
                python_pending_journals(&events),
                json!({}),
                "Python pending view for {case}"
            );
            let replayed = replay_stream(&events, None, |_| Ok(()))
                .unwrap_or_else(|error| panic!("replay {case}: {error}"));
            let pending = replayed
                .pending_operations
                .iter()
                .map(|pending| (pending.kind, pending.journal_path.as_deref()))
                .collect::<Vec<_>>();
            match outcome {
                CreateImportOutcome::Aborted => assert_eq!(
                    pending,
                    [(PendingOperationKind::Recovery, Some(journal_path))],
                    "{case}"
                ),
                _ => assert!(pending.is_empty(), "{case} left {pending:?}"),
            }
            let records = match outcome {
                CreateImportOutcome::Aborted => vec![],
                _ => vec![record],
            };
            let (config, evidence) =
                write_policy_inputs(&scratch, &records, "2026-09-22T09:40:01+00:00", |_| {});
            let decisions =
                evaluate_policy_at(&events, &config, &evidence, "2026-09-22T09:40:01+00:00")
                    .unwrap_or_else(|error| panic!("evaluate {case}: {error}"));
            match outcome {
                // No row owns whatever storage the operation may have left,
                // so the slot stays a blocked observer subject.
                CreateImportOutcome::Aborted => {
                    assert_eq!(decisions.len(), 1, "{case}");
                    assert_eq!(decisions[0].verdict, Verdict::Blocked, "{case}");
                    assert_eq!(decisions[0].generation, None, "{case}");
                    assert_eq!(
                        decisions[0].reason_codes,
                        ["ACTIVE_RECORD_MISSING", "RECOVERY_PENDING"],
                        "{case}"
                    );
                }
                _ => {
                    assert_eq!(decisions.len(), 1, "{case}");
                    for reason in ["RECOVERY_PENDING", "OPERATION_JOURNAL_PENDING"] {
                        assert!(
                            !decisions[0].reason_codes.iter().any(|code| code == reason),
                            "{case}: {:?}",
                            decisions[0].reason_codes
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn a_later_create_of_the_same_slot_closes_a_row_less_recovery_marker() {
    // Python's create refuses an existing slot path, so a later create of the
    // same slot type that publishes a row owning every checkout path the
    // aborted attempt planned proves that attempt left no slot directory.
    // Both journal paths are reused per slot.
    for journal_path in ["CREATE.6.node-a.6.slot-a.journal", "ACTIVE.node-a.journal"] {
        let scratch = Scratch::new();
        let events = scratch.events();
        let record = active_record("slot-a", 1);
        let progress = json!({
            "slot": "slot-a",
            "operation": "create",
            "journal_path": journal_path,
            "journal": attempt_journal("create", &record),
        });
        let completion =
            json!({"slot": "slot-a", "operation": "create", "journal_path": journal_path});
        let mut tip = import_log(&events, vec![], vec![]);
        tip = append_event(
            &events,
            2,
            &tip,
            "operation-progress-recorded",
            progress.clone(),
        );
        tip = append_event(
            &events,
            3,
            &tip,
            "recovery-started",
            recovery_started_payload("slot-a", "create"),
        );
        tip = append_event(&events, 4, &tip, "operation-completed", completion.clone());
        let pending = replay_stream(&events, None, |_| Ok(()))
            .unwrap_or_else(|error| panic!("replay abort at {journal_path}: {error}"))
            .pending_operations
            .iter()
            .map(|pending| (pending.kind, pending.journal_path.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            pending,
            [(
                PendingOperationKind::Recovery,
                Some(journal_path.to_owned())
            )],
            "abort at {journal_path}"
        );

        tip = append_event(&events, 5, &tip, "operation-progress-recorded", progress);
        tip = append_event(
            &events,
            6,
            &tip,
            "active-state-recorded",
            json!({
                "action": "slot-created",
                "slot": "slot-a",
                "previous_revision": 0,
                "revision": 1,
                "previous_record_sha256": null,
                "record": record.clone(),
                "evidence": {},
            }),
        );
        append_event(&events, 7, &tip, "operation-completed", completion);
        assert_eq!(
            python_pending_journals(&events),
            json!({}),
            "{journal_path}"
        );
        let replayed = replay_stream(&events, None, |_| Ok(()))
            .unwrap_or_else(|error| panic!("replay re-create at {journal_path}: {error}"));
        assert!(
            replayed.pending_operations.is_empty(),
            "re-create at {journal_path} left {:?}",
            replayed.pending_operations
        );
        let (config, evidence) = write_policy_inputs(
            &scratch,
            std::slice::from_ref(&record),
            "2026-09-22T09:40:01+00:00",
            |_| {},
        );
        let decisions =
            evaluate_policy_at(&events, &config, &evidence, "2026-09-22T09:40:01+00:00")
                .unwrap_or_else(|error| panic!("evaluate {journal_path}: {error}"));
        // The re-created slot decides exactly like the same row with no
        // aborted attempt in its history.
        let control = Scratch::new();
        let control_events = control.events();
        import_log(&control_events, vec![record.clone()], vec![]);
        let (control_config, control_evidence) =
            write_policy_inputs(&control, &[record], "2026-09-22T09:40:01+00:00", |_| {});
        let expected = evaluate_policy_at(
            &control_events,
            &control_config,
            &control_evidence,
            "2026-09-22T09:40:01+00:00",
        )
        .expect("evaluate control");
        assert_eq!(decisions.len(), 1, "{journal_path}");
        assert_eq!(expected.len(), 1, "control");
        assert_eq!(decisions[0].verdict, expected[0].verdict, "{journal_path}");
        assert_eq!(
            decisions[0].reason_codes, expected[0].reason_codes,
            "{journal_path}"
        );
        assert!(
            !decisions[0]
                .reason_codes
                .iter()
                .any(|code| code == "RECOVERY_PENDING"),
            "{journal_path}: {:?}",
            decisions[0].reason_codes
        );
    }
}

#[test]
fn create_recovery_marker_closes_only_on_its_bound_journal() {
    // A recovery that loads an already-completed legacy singleton journal is
    // bound to that path. A later create of the same slot at the scoped path
    // publishes a row owning the same storage, yet completing that other
    // journal does not prove the singleton attempt ended.
    let scratch = Scratch::new();
    let events = scratch.events();
    let create_path = "CREATE.6.node-a.6.slot-a.journal";
    let singleton = "ACTIVE.node-a.journal";
    let record = active_record("slot-a", 1);
    let progress = |journal_path: &str| {
        json!({
            "slot": "slot-a",
            "operation": "create",
            "journal_path": journal_path,
            "journal": attempt_journal("create", &record),
        })
    };
    let completion = |journal_path: &str| json!({"slot": "slot-a", "operation": "create", "journal_path": journal_path});
    let mut tip = import_log(&events, vec![], vec![]);
    tip = append_event(
        &events,
        2,
        &tip,
        "operation-progress-recorded",
        progress(singleton),
    );
    tip = append_event(
        &events,
        3,
        &tip,
        "operation-completed",
        completion(singleton),
    );
    tip = append_event(
        &events,
        4,
        &tip,
        "recovery-started",
        recovery_started_payload("slot-a", "create"),
    );
    tip = append_event(
        &events,
        5,
        &tip,
        "operation-progress-recorded",
        progress(create_path),
    );
    tip = append_event(
        &events,
        6,
        &tip,
        "active-state-recorded",
        json!({
            "action": "slot-created",
            "slot": "slot-a",
            "previous_revision": 0,
            "revision": 1,
            "previous_record_sha256": null,
            "record": record.clone(),
            "evidence": {},
        }),
    );
    append_event(
        &events,
        7,
        &tip,
        "operation-completed",
        completion(create_path),
    );
    assert_eq!(python_pending_journals(&events), json!({}));
    let pending = replay_stream(&events, None, |_| Ok(()))
        .expect("valid replay")
        .pending_operations;
    let kinds = pending
        .iter()
        .map(|pending| (pending.kind, pending.journal_path.as_deref()))
        .collect::<Vec<_>>();
    assert_eq!(kinds, [(PendingOperationKind::Recovery, Some(singleton))]);
}

#[test]
fn a_later_recovery_keeps_an_open_marker_whose_storage_its_row_does_not_own() {
    // A recovery marker bound to one journal must survive a later recovery of
    // the same slot and operation. Through another journal it stays distinct;
    // through the same journal the new row must own both attempts' storage.
    let singleton = "ACTIVE.node-a.journal";
    let create_path = "CREATE.6.node-a.6.slot-a.journal";
    let agent_record = active_record("slot-a", 1);
    let validate_record = active_record_for("slot-a", "agent-slot-a", 1, "validate");
    let imported = imported_active_record("slot-a", "agent-slot-a", 1);
    let mut wider_import = imported.clone();
    let mut docs = imported["checkouts"][0].clone();
    docs["name"] = json!("docs");
    docs["path"] = json!("worktrees/slots/slot-a/docs");
    wider_import["checkouts"]
        .as_array_mut()
        .expect("imported checkouts")
        .push(docs);
    let progress = |operation: &str, path: &str, record: &Value| {
        (
            "operation-progress-recorded",
            json!({
                "slot": "slot-a",
                "operation": operation,
                "journal_path": path,
                "journal": attempt_journal(operation, record),
            }),
        )
    };
    let recovery = |operation: &str| {
        (
            "recovery-started",
            recovery_started_payload("slot-a", operation),
        )
    };
    let completed = |operation: &str, path: &str| {
        (
            "operation-completed",
            json!({"slot": "slot-a", "operation": operation, "journal_path": path}),
        )
    };
    let row = |action: &str, record: &Value| {
        (
            "active-state-recorded",
            json!({
                "action": action,
                "slot": "slot-a",
                "previous_revision": 0,
                "revision": 1,
                "previous_record_sha256": null,
                "record": record.clone(),
                "evidence": {},
            }),
        )
    };
    let validate_recreate = [
        progress("create", create_path, &validate_record),
        recovery("create"),
        row("slot-created", &validate_record),
        completed("create", create_path),
    ];
    let open_singleton = vec![(PendingOperationKind::Recovery, Some(singleton.to_owned()))];
    for (case, history, final_record, expected) in [
        (
            "legacy singleton marker, then a validate create",
            [vec![recovery("create")], validate_recreate.to_vec()].concat(),
            &validate_record,
            open_singleton.clone(),
        ),
        (
            "agent singleton attempt, then a validate create",
            [
                vec![
                    progress("create", singleton, &agent_record),
                    recovery("create"),
                    completed("create", singleton),
                ],
                validate_recreate.to_vec(),
            ]
            .concat(),
            &validate_record,
            open_singleton.clone(),
        ),
        // A known false blocker: Python's publication check proved the slot
        // held no `docs` directory, but replay compares paths only.
        (
            "import recovered again without a checkout",
            vec![
                progress("import-existing", singleton, &wider_import),
                recovery("import-existing"),
                completed("import-existing", singleton),
                progress("import-existing", singleton, &imported),
                recovery("import-existing"),
                row("slot-imported-by-recovery", &imported),
                completed("import-existing", singleton),
            ],
            &imported,
            open_singleton.clone(),
        ),
        (
            "import recovered again with the same checkouts",
            vec![
                progress("import-existing", singleton, &imported),
                recovery("import-existing"),
                completed("import-existing", singleton),
                progress("import-existing", singleton, &imported),
                recovery("import-existing"),
                row("slot-imported-by-recovery", &imported),
                completed("import-existing", singleton),
            ],
            &imported,
            vec![],
        ),
    ] {
        let scratch = Scratch::new();
        let events = scratch.events();
        let mut tip = import_log(&events, vec![], vec![]);
        for (offset, (kind, payload)) in history.into_iter().enumerate() {
            tip = append_event(&events, offset as u64 + 2, &tip, kind, payload);
        }
        assert_eq!(python_pending_journals(&events), json!({}), "{case}");
        let pending = replay_stream(&events, None, |_| Ok(()))
            .unwrap_or_else(|error| panic!("replay {case}: {error}"))
            .pending_operations
            .iter()
            .map(|pending| (pending.kind, pending.journal_path.clone()))
            .collect::<Vec<_>>();
        assert_eq!(pending, expected, "{case}");
        let (config, evidence) = write_policy_inputs(
            &scratch,
            std::slice::from_ref(final_record),
            "2026-09-22T09:40:01+00:00",
            |_| {},
        );
        let decisions =
            evaluate_policy_at(&events, &config, &evidence, "2026-09-22T09:40:01+00:00")
                .unwrap_or_else(|error| panic!("evaluate {case}: {error}"));
        assert_eq!(decisions.len(), 1, "{case}");
        assert_eq!(
            decisions[0]
                .reason_codes
                .iter()
                .any(|code| code == "RECOVERY_PENDING"),
            !expected.is_empty(),
            "{case}: {:?}",
            decisions[0].reason_codes
        );
        if !expected.is_empty() {
            assert_eq!(decisions[0].verdict, Verdict::Blocked, "{case}");
        }
    }
}

#[test]
fn a_zero_checkout_import_recovery_closes_its_marker() {
    // Python's historical import skips checkouts that no longer exist, so an
    // import can publish a row with no checkouts. That import placed nothing,
    // and its recovery must close like any other.
    let scratch = Scratch::new();
    let events = scratch.events();
    let journal_path = "ACTIVE.node-a.journal";
    let mut record = imported_active_record("slot-a", "agent-slot-a", 1);
    record["checkouts"] = json!([]);
    let mut tip = import_log(&events, vec![], vec![]);
    tip = append_event(
        &events,
        2,
        &tip,
        "operation-progress-recorded",
        json!({
            "slot": "slot-a", "operation": "import-existing", "journal_path": journal_path,
            "journal": attempt_journal("import-existing", &record),
        }),
    );
    tip = append_event(
        &events,
        3,
        &tip,
        "recovery-started",
        recovery_started_payload("slot-a", "import-existing"),
    );
    tip = append_event(
        &events,
        4,
        &tip,
        "active-state-recorded",
        json!({
            "action": "slot-imported-by-recovery", "slot": "slot-a",
            "previous_revision": 0, "revision": 1, "previous_record_sha256": null,
            "record": record, "evidence": {},
        }),
    );
    append_event(
        &events,
        5,
        &tip,
        "operation-completed",
        json!({"slot": "slot-a", "operation": "import-existing", "journal_path": journal_path}),
    );
    assert_eq!(python_pending_journals(&events), json!({}));
    let replayed = replay_stream(&events, None, |_| Ok(())).expect("replay");
    assert_eq!(replayed.active_metadata["slot-a"].checkouts, vec![]);
    assert!(
        replayed.pending_operations.is_empty(),
        "{:?}",
        replayed.pending_operations
    );
}

#[test]
fn a_later_create_that_does_not_own_the_aborted_storage_keeps_the_recovery_marker() {
    // Python derives the refused slot path from the new create's slot type,
    // so a create of the same slot name under the other slot type proves
    // nothing about the aborted attempt's directory. A row owning only some
    // of the attempt's planned checkouts, or an attempt whose journal does
    // not identify its storage, likewise leaves storage no row accounts for.
    let create_path = "CREATE.6.node-a.6.slot-a.journal";
    let agent_record = active_record("slot-a", 1);
    let validate_record = active_record_for("slot-a", "agent-slot-a", 1, "validate");
    let mut wider = attempt_journal("create", &agent_record);
    wider["planned"]
        .as_array_mut()
        .expect("planned checkouts")
        .push(json!({"name": "docs", "destination": "worktrees/slots/slot-a/docs"}));
    // The same checkout paths under the other slot type isolate the slot-type
    // comparison from the path comparison.
    let mut validate_at_agent_paths = validate_record.clone();
    validate_at_agent_paths["checkouts"] = agent_record["checkouts"].clone();
    let mut unidentified = attempt_journal("create", &agent_record);
    unidentified
        .as_object_mut()
        .expect("journal object")
        .remove("planned");
    for (case, aborted_journal, recreated) in [
        (
            "other slot type",
            attempt_journal("create", &agent_record),
            &validate_record,
        ),
        (
            "same paths, other slot type",
            attempt_journal("create", &agent_record),
            &validate_at_agent_paths,
        ),
        ("unowned planned checkout", wider, &agent_record),
        ("unidentified storage", unidentified, &agent_record),
    ] {
        let scratch = Scratch::new();
        let events = scratch.events();
        let completion =
            json!({"slot": "slot-a", "operation": "create", "journal_path": create_path});
        let mut tip = import_log(&events, vec![], vec![]);
        tip = append_event(
            &events,
            2,
            &tip,
            "operation-progress-recorded",
            json!({
                "slot": "slot-a",
                "operation": "create",
                "journal_path": create_path,
                "journal": aborted_journal,
            }),
        );
        tip = append_event(
            &events,
            3,
            &tip,
            "recovery-started",
            recovery_started_payload("slot-a", "create"),
        );
        tip = append_event(&events, 4, &tip, "operation-completed", completion.clone());
        tip = append_event(
            &events,
            5,
            &tip,
            "operation-progress-recorded",
            json!({
                "slot": "slot-a",
                "operation": "create",
                "journal_path": create_path,
                "journal": attempt_journal("create", recreated),
            }),
        );
        tip = append_event(
            &events,
            6,
            &tip,
            "active-state-recorded",
            json!({
                "action": "slot-created",
                "slot": "slot-a",
                "previous_revision": 0,
                "revision": 1,
                "previous_record_sha256": null,
                "record": recreated.clone(),
                "evidence": {},
            }),
        );
        append_event(&events, 7, &tip, "operation-completed", completion);
        assert_eq!(python_pending_journals(&events), json!({}), "{case}");
        let pending = replay_stream(&events, None, |_| Ok(()))
            .unwrap_or_else(|error| panic!("replay {case}: {error}"))
            .pending_operations
            .iter()
            .map(|pending| (pending.kind, pending.journal_path.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            pending,
            [(PendingOperationKind::Recovery, Some(create_path.to_owned()))],
            "{case}"
        );
        let (config, evidence) = write_policy_inputs(
            &scratch,
            std::slice::from_ref(recreated),
            "2026-09-22T09:40:01+00:00",
            |_| {},
        );
        let decisions =
            evaluate_policy_at(&events, &config, &evidence, "2026-09-22T09:40:01+00:00")
                .unwrap_or_else(|error| panic!("evaluate {case}: {error}"));
        assert_eq!(decisions.len(), 1, "{case}");
        assert_eq!(decisions[0].verdict, Verdict::Blocked, "{case}");
        assert!(
            decisions[0]
                .reason_codes
                .iter()
                .any(|code| code == "RECOVERY_PENDING"),
            "{case}: {:?}",
            decisions[0].reason_codes
        );
    }
}

#[test]
fn finish_recovery_marker_survives_a_rollback_completion() {
    // Negative control: finish rollbacks (`_rollback_path_fence`,
    // `_begin_finish`) complete the journal while deliberately retaining the
    // slot, so completion alone must not close a finish recovery attempt.
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = scoped_active_record("slot-a", 1);
    let finish_path = "FINISH.6.node-a.6.slot-a.journal";
    let mut tip = import_log(&events, vec![record.clone()], vec![]);
    tip = append_event(
        &events,
        2,
        &tip,
        "operation-progress-recorded",
        json!({
            "slot": "slot-a",
            "operation": "finish",
            "journal_path": finish_path,
            "journal": {"schema": 2, "kind": "finish", "machine": "node-a", "slot": "slot-a"},
        }),
    );
    tip = append_event(
        &events,
        3,
        &tip,
        "recovery-started",
        recovery_started_payload("slot-a", "finish"),
    );
    append_event(
        &events,
        4,
        &tip,
        "operation-completed",
        json!({"slot": "slot-a", "operation": "finish", "journal_path": finish_path}),
    );
    assert_eq!(python_pending_journals(&events), json!({}));
    let pending = replay_stream(&events, None, |_| Ok(()))
        .expect("valid replay")
        .pending_operations;
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].kind, PendingOperationKind::Recovery);
    assert_eq!(pending[0].journal_path.as_deref(), Some(finish_path));
    let (config, evidence) =
        write_policy_inputs(&scratch, &[record], "2026-09-22T09:40:01+00:00", |_| {});
    let decision = evaluate_policy_at(&events, &config, &evidence, "2026-09-22T09:40:01+00:00")
        .expect("evaluate retained slot")
        .remove(0);
    assert_eq!(decision.verdict, Verdict::Blocked);
    assert!(
        decision
            .reason_codes
            .iter()
            .any(|code| code == "RECOVERY_PENDING"),
        "{:?}",
        decision.reason_codes
    );
}

/// Interrupts a real Python create or import, completes it through Python's
/// own `recover`, and reports the event log with Python's pending view.
fn python_recovered_create_or_import(
    root: &Path,
    operation: &str,
    mode: &str,
) -> std::process::Output {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repository root");
    let script = r#"
import contextlib
import io
import json
import os
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
sys.path.insert(0, sys.argv[2])
from wrkslots import cli as w
import test_lifecycle as t
os.environ['WRKSLOTS_MACHINE'] = 'testhost'
root, operation, mode = Path(sys.argv[3]), sys.argv[4], sys.argv[5]
project, repository, _remote = t.make_project(root)
if operation == 'create':
    point = 'after-active-write' if mode == 'durable' else 'after-create-worktree'
    interrupted = t.create(project, env={'WRKSLOTS_TEST_INTERRUPT': point})
else:
    point = 'after-import-active-write' if mode == 'durable' else 'after-import-journal'
    tree = t.checkout(project)
    tree.parent.mkdir()
    t.git(repository, 'worktree', 'add', '-b', 'codex/imported', str(tree), 'origin/main')
    interrupted = t.command(
        project, 'import-existing', 'slot01', '--agent', 'codex-1',
        '--task', 'task-import', '--purpose', 'interrupted import',
        '--repo', 'product=repo', '--apply', '--verified-live',
        '--owner-pid', str(os.getpid()), '--coordinator-pid', str(os.getpid()),
        env={'WRKSLOTS_TEST_INTERRUPT': point},
    )
assert interrupted.returncode == 86, interrupted.stderr
argv = ['--project-root', str(project), 'recover', '--coordinator-authorized',
        '--coordinator-pid', str(os.getpid())]
if mode == 'abort':
    argv.append('--abort-create' if operation == 'create' else '--abort-import')
captured = io.StringIO()
with contextlib.redirect_stdout(captured), contextlib.redirect_stderr(captured):
    rc = w.main(argv)
assert rc == 0, captured.getvalue()
config = w._load_config(str(project), 'testhost')
pending = w._pending_operations_from_events(config, 'testhost')
active, _archive = w._states_from_events(config, 'testhost', require_repository=False)
print(json.dumps({
    'events': str(config.control / 'EVENTS.testhost'),
    'pending': {path.name: journal.get('kind') for path, journal in pending.items()},
    'kinds': [event['kind'] for event in w._load_events(config)],
    'active_slots': sorted(record.slot for record in active.slots),
}, sort_keys=True, separators=(',', ':')))
"#;
    python_command()
        .arg("-c")
        .arg(script)
        .arg(repository.join("py"))
        .arg(repository.join("py/wrkslots/tests"))
        .arg(root)
        .arg(operation)
        .arg(mode)
        .output()
        .expect("run recovered Python create or import fixture")
}

#[test]
fn real_python_create_and_import_recoveries_close_their_marker_only_with_a_row() {
    // End-to-end counterpart of the synthetic matrix: histories written by
    // Python's own interrupted create/import and `recover`, not by the test.
    let cases = ["create", "import-existing"]
        .into_iter()
        .flat_map(|operation| {
            ["durable", "republish", "abort"].map(|mode| (operation, mode, Scratch::new()))
        })
        .collect::<Vec<_>>();
    // Each fixture builds its own Git project; run them concurrently.
    let outputs = std::thread::scope(|scope| {
        cases
            .iter()
            .map(|(operation, mode, scratch)| {
                scope.spawn(move || python_recovered_create_or_import(&scratch.0, operation, mode))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("fixture thread"))
            .collect::<Vec<_>>()
    });
    for ((operation, mode, _scratch), output) in cases.iter().zip(outputs) {
        let case = format!("{operation} {mode}");
        assert!(
            output.status.success(),
            "Python fixture {case} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let oracle: Value = serde_json::from_slice(&output.stdout).expect("parse fixture");
        assert_eq!(
            oracle["pending"],
            json!({}),
            "Python pending view for {case}"
        );
        let kinds = oracle["kinds"].as_array().expect("event kinds");
        assert!(
            kinds.contains(&json!("recovery-started"))
                && kinds.last() == Some(&json!("operation-completed")),
            "{case}: {kinds:?}"
        );
        let expected_active: &[&str] = if *mode == "abort" { &[] } else { &["slot01"] };
        assert_eq!(oracle["active_slots"], json!(expected_active), "{case}");
        let events = PathBuf::from(oracle["events"].as_str().expect("events path"));
        let replayed = replay_stream(&events, None, |_| Ok(()))
            .unwrap_or_else(|error| panic!("replay {case}: {error}"));
        let expected_count: u64 = if *mode == "abort" { 0 } else { 1 };
        assert_eq!(replayed.summary.active_count, expected_count, "{case}");
        let pending = replayed
            .pending_operations
            .iter()
            .map(|pending| {
                (
                    pending.kind,
                    pending.slot.as_str(),
                    pending.operation.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        if *mode == "abort" {
            assert_eq!(
                pending,
                [(PendingOperationKind::Recovery, "slot01", Some(*operation))],
                "{case}"
            );
        } else {
            assert!(pending.is_empty(), "{case} left {pending:?}");
        }
    }
}

#[test]
fn plain_rebuild_after_a_policy_rebuild_requires_a_fresh_index() -> Result<(), crate::ObserverError>
{
    let scratch = Scratch::new();
    let events = scratch.events();
    let record = scoped_active_record("slot-a", 1);
    import_log(&events, vec![record.clone()], vec![]);
    let (config, evidence) =
        write_policy_inputs(&scratch, &[record], "2026-09-22T09:40:01+00:00", |_| {});
    let index = scratch.0.join("policy-then-plain.sqlite");
    rebuild_policy_index(&events, &index, &config, &evidence)?;

    let error = rebuild_index(&events, &index)
        .expect_err("plain rebuild must not discard indexed policy evidence")
        .to_string();
    assert!(error.contains("high-water mark"), "{error}");
    assert!(error.contains("delete the disposable index"), "{error}");
    let retained = read_decision(&index, "slot-a")?;
    assert!(retained.evidence_sha256.is_some());
    assert_eq!(retained.reason_codes, with_rollout_gates(&[]));

    fs::remove_file(&index).expect("remove disposable policy index");
    rebuild_index(&events, &index)?;
    let plain = read_decision(&index, "slot-a")?;
    assert_eq!(plain.verdict, Verdict::Unknown);
    assert_eq!(plain.evidence_sha256, None);
    assert_eq!(plain.reason_codes, ["POLICY_INPUTS_MISSING"]);
    Ok(())
}

#[test]
fn zero_second_offsets_discard_their_fraction_like_python() {
    // CPython's `tzinfo_from_isoformat_results` returns UTC whenever the
    // whole-second offset is zero, dropping any fractional offset seconds.
    let values = [
        "2026-W39-2T10:30:45+00.3045",
        "0001-W01-1+034628+00,77551173",
        "99991231 14055036+00.2916815",
        "2026-09-22T10:00:00-00:00:00.5",
        "2026-09-22T10:00:00+00:00,9",
        "2026-09-22T10:00:00-00:00:01.5",
    ]
    .map(str::to_owned);
    let python = python_timestamp_instants(&values);
    assert_eq!(python[0], 1_790_073_045_000_000);
    for (value, python_microseconds) in values.iter().zip(python) {
        let rust = crate::schema::parse_timestamp_instant(value, "fixture timestamp")
            .unwrap_or_else(|error| panic!("{value:?}: {error}"));
        assert_eq!(
            rust.timestamp_micros(),
            python_microseconds,
            "Rust/Python instant mismatch for {value:?}"
        );
    }
}

#[test]
fn timestamp_instants_match_python_for_every_accepted_matrix_value() {
    let (accepted_count, mut values) = python_timestamp_matrix();
    values.truncate(accepted_count);
    let python = python_timestamp_instants(&values);
    assert_eq!(python.len(), accepted_count);
    for (value, python_microseconds) in values.iter().zip(python) {
        let rust = crate::schema::parse_timestamp_instant(value, "fixture timestamp")
            .unwrap_or_else(|error| panic!("{value:?}: {error}"));
        assert_eq!(
            rust.timestamp_micros(),
            python_microseconds,
            "Rust/Python instant mismatch for {value:?}"
        );
    }
}
