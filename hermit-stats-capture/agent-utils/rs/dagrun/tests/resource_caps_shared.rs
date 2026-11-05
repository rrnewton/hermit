//! Two-process proof that `resource_caps` can protect one capacity across runner invocations.
//!
//! The first arm deliberately omits the shared path and forces both processes to read the same
//! counter value before either writes. The second arm supplies one path to both processes and
//! proves that the second command waits outside its one-second step deadline.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dagrun::resource_caps::PATH_ENV;

struct Fixture {
    dir: PathBuf,
    holder_dag: PathBuf,
    writer_dag: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "dagrun-resource-caps-processes-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("counter");
        let observations = dir.join("observations");
        let holder_command = format!(
            "test -z \"${{DAGRUN_RESOURCE_CAPS_PATH+x}}\"; value=$(cat {counter}); printf '%s' \"$value\" >> {observations}; while [ ! -e {release} ]; do sleep 0.02; done; printf '%s\\n' \"$((value + 1))\" > {counter}",
            counter = counter.display(),
            observations = observations.display(),
            release = dir.join("release").display(),
        );
        let writer_command = format!(
            "test -z \"${{DAGRUN_RESOURCE_CAPS_PATH+x}}\"; value=$(cat {counter}); printf '%s' \"$value\" >> {observations}; printf '%s\\n' \"$((value + 1))\" > {counter}",
            counter = counter.display(),
            observations = observations.display(),
        );
        let holder_dag = dir.join("holder.json");
        std::fs::write(
            &holder_dag,
            serde_json::to_vec(&serde_json::json!({
                "resource_caps": {"guest": 1},
                "steps": [{
                    "group": "g",
                    "job": "holder",
                    "cmd": holder_command,
                    "timeout": 10,
                    "cpu_timeout": 10,
                    "hint": {"resources": {"guest": 1}}
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let writer_dag = dir.join("writer.json");
        std::fs::write(
            &writer_dag,
            serde_json::to_vec(&serde_json::json!({
                "resource_caps": {"guest": 1},
                "steps": [{
                    "group": "g",
                    "job": "writer",
                    "cmd": writer_command,
                    "timeout": 1,
                    "cpu_timeout": 10,
                    "hint": {"resources": {"guest": 1}}
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        Self {
            dir,
            holder_dag,
            writer_dag,
        }
    }

    fn reset(&self) {
        std::fs::write(self.dir.join("counter"), b"0\n").unwrap();
        std::fs::write(self.dir.join("observations"), b"").unwrap();
        let _ = std::fs::remove_file(self.dir.join("release"));
    }

    fn initialize_state(path: &Path) {
        std::fs::write(path, br#"{"next_ticket":0,"requests":[]}"#).unwrap();
    }

    fn spawn(&self, dag: &Path, resource_caps_path: Option<&Path>) -> Child {
        let mut command = self.command(dag);
        if let Some(path) = resource_caps_path {
            command.args(["--resource-caps-path", path.to_str().unwrap()]);
        }
        command.spawn().unwrap()
    }

    fn spawn_with_environment(&self, dag: &Path, resource_caps_path: &Path) -> Child {
        let mut command = self.command(dag);
        command.env(PATH_ENV, resource_caps_path).spawn().unwrap()
    }

    fn command(&self, dag: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dagrun"));
        command
            .args([
                "run",
                "--dag",
                dag.to_str().unwrap(),
                "-j",
                "1",
                "--no-profile",
                "--no-profile-feedback",
                "--unsafe-no-cgroups",
            ])
            .env("DAGRUN_NO_LOGS", "1")
            .env_remove(PATH_ENV)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn wait_for_first_read(&self) {
        let path = self.dir.join("observations");
        let deadline = Instant::now() + Duration::from_secs(3);
        while std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0) == 0 {
            assert!(
                Instant::now() < deadline,
                "the first process never entered the critical section"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn text(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn two_processes_collide_without_the_fix_and_wait_with_it() {
    let fixture = Fixture::new();

    // REMOVED FIX: each process enforces guest=1 only against itself. The second process starts
    // after the first has read 0, both commands overlap, both read 0, and one update is lost.
    fixture.reset();
    let first = fixture.spawn(&fixture.holder_dag, None);
    fixture.wait_for_first_read();
    let second = fixture.spawn(&fixture.writer_dag, None);
    let second = second.wait_with_output().unwrap();
    assert!(second.status.success(), "{}", text(&second));
    std::fs::write(fixture.dir.join("release"), b"go\n").unwrap();
    let first = first.wait_with_output().unwrap();
    assert!(first.status.success(), "{}", text(&first));
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("observations")).unwrap(),
        "00"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("counter")).unwrap(),
        "1\n"
    );

    // FIXED: both processes use the same resource_caps path. The second process is launched while
    // the first owns guest=1, reports that wait, and still passes a one-second STEP deadline even
    // though its total process wall time includes the first process's critical section.
    fixture.reset();
    let ledger = fixture.dir.join("resource-caps.json");
    Fixture::initialize_state(&ledger);
    let first = fixture.spawn(&fixture.holder_dag, Some(&ledger));
    fixture.wait_for_first_read();
    let second_started = Instant::now();
    let mut second = fixture.spawn(&fixture.writer_dag, Some(&ledger));
    std::thread::sleep(Duration::from_millis(1250));
    assert!(
        second.try_wait().unwrap().is_none(),
        "the one-second writer should still be waiting outside its own step timeout"
    );
    std::fs::write(fixture.dir.join("release"), b"go\n").unwrap();
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    let second_wall = second_started.elapsed().as_secs_f64();
    assert!(first.status.success(), "{}", text(&first));
    assert!(second.status.success(), "{}", text(&second));
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("observations")).unwrap(),
        "01"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("counter")).unwrap(),
        "2\n"
    );
    let second_text = text(&second);
    assert!(
        second_text.contains("WAIT resource_caps guest=1")
            && second_text.contains("READY resource_caps after")
            && second_text.contains(&format!(
                "resource_caps shared across scheduler processes: {}",
                ledger.display()
            )),
        "the collision was not observed and handled:\n{second_text}"
    );
    assert!(
        second_wall > 1.0,
        "the second process did not include a real pre-step wait: {second_wall:.3}s"
    );
}

#[test]
fn malformed_shared_state_is_refused_before_the_command_runs() {
    let fixture = Fixture::new();
    fixture.reset();
    let ledger = fixture.dir.join("resource-caps.json");
    std::fs::write(&ledger, b"{not json\n").unwrap();

    let output = fixture
        .spawn(&fixture.writer_dag, Some(&ledger))
        .wait_with_output()
        .unwrap();
    let output_text = text(&output);
    assert!(!output.status.success(), "{output_text}");
    assert!(
        output_text.contains("REFUSING to run before any node starts")
            && output_text.contains("is corrupt"),
        "the malformed state was not refused by name:\n{output_text}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("observations")).unwrap(),
        "",
        "the command ran despite malformed shared state"
    );
}

#[test]
fn missing_configured_state_is_refused_before_the_command_runs() {
    let fixture = Fixture::new();
    fixture.reset();
    let ledger = fixture.dir.join("missing-resource-caps.json");

    let output = fixture
        .spawn(&fixture.writer_dag, Some(&ledger))
        .wait_with_output()
        .unwrap();
    let output_text = text(&output);
    assert!(!output.status.success(), "{output_text}");
    assert!(
        output_text.contains("REFUSING to run before any node starts")
            && output_text.contains("does not exist"),
        "the absent configured state was not refused by name:\n{output_text}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("observations")).unwrap(),
        "",
        "the command ran despite an absent configured state file"
    );
    assert!(!ledger.exists(), "the missing state was silently created");
}

#[test]
fn environment_path_is_supported_and_an_explicit_flag_wins() {
    let fixture = Fixture::new();
    fixture.reset();
    let environment_ledger = fixture.dir.join("environment-resource-caps.json");
    Fixture::initialize_state(&environment_ledger);
    let environment = fixture
        .spawn_with_environment(&fixture.writer_dag, &environment_ledger)
        .wait_with_output()
        .unwrap();
    let environment_text = text(&environment);
    assert!(environment.status.success(), "{environment_text}");
    assert!(
        environment_text.contains(&environment_ledger.display().to_string()),
        "the environment-selected path was not disclosed:\n{environment_text}"
    );

    fixture.reset();
    let flag_ledger = fixture.dir.join("flag-resource-caps.json");
    Fixture::initialize_state(&flag_ledger);
    let missing_environment_ledger = fixture.dir.join("missing-environment.json");
    let mut command = fixture.command(&fixture.writer_dag);
    let flag = command
        .args(["--resource-caps-path", flag_ledger.to_str().unwrap()])
        .env(PATH_ENV, &missing_environment_ledger);
    let explicit = flag.output().unwrap();
    let explicit_text = text(&explicit);
    assert!(explicit.status.success(), "{explicit_text}");
    assert!(
        explicit_text.contains(&flag_ledger.display().to_string()),
        "the flag-selected path was not disclosed:\n{explicit_text}"
    );
    assert!(
        !missing_environment_ledger.exists(),
        "the environment path was used even though the flag was present"
    );
}
