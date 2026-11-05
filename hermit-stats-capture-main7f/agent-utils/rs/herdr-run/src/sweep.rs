//! Collect the evidence [`crate::reap`] decides on, from the run spool and the live session.
//!
//! [`crate::reap`] deliberately holds no I/O, so that its policy can be exercised against
//! planted-stale and planted-live populations without a Herdr server. This module is the other
//! half: it gathers the evidence and hands it over. Keeping the two apart is what makes "the reaper
//! spared everything" testable rather than a claim.
//!
//! # Where the candidate set comes from
//!
//! From **our own run spool**, not from `herdr pane list`. Every run herdr-run has performed wrote
//! a `meta.json` naming the pane it used, the workspace and tab labels it resolved, and the agent
//! it ran for. That is a record we authored, so a pane appearing in it is a pane *this tool*
//! opened — which is exactly the population the tab leak is made of. Enumerating live panes instead
//! would sweep up tabs a human opened in the same workspace and make scope a matter of
//! pattern-matching a label.
//!
//! **And the candidate set is bounded by our own retention.** [`crate::retention`] deletes a run
//! directory, `meta.json` included, `retention_days` (default 4) after the run finished. A tab whose
//! owning agent last ran a week ago therefore has no surviving record and is not a candidate at all
//! — while still holding a pane and still counting against `max_panes`. The oldest leaks, which are
//! the ones the cap exists to bound, are precisely the ones this sweep cannot see. That is a real
//! gap, not a rounding error, and it is why `herdr-run reap` prints the window alongside the counts
//! instead of letting "considered: 3" imply it looked at everything. Closing it properly needs a
//! pane ledger that outlives output retention, which is a separate change; widening the candidate
//! set to `herdr pane list` is NOT the answer, because scope would then be a matter of
//! pattern-matching a label on tabs we may not have opened.
//!
//! # Scope is re-derived, not read back
//!
//! A recorded tab label is not taken as proof that the tab is ours. The label is recomputed from
//! the CURRENT configuration and the recorded agent name, and must match exactly; the recorded
//! workspace label must equal the configured one. So retargeting `workspace` or `tab_name`
//! immediately takes the old tabs out of scope, rather than leaving this tool authorised over tabs
//! it would no longer create.
//!
//! # What "the shell is gone" means here
//!
//! Herdr still lists the pane, and `pane process-info` still names a shell pid, but `/proc` says no
//! such process exists. That is the husk case: the tab outlived the shell that owned it. Any other
//! outcome — `/proc` unreadable, the pane no longer listed, the control call failing — is UNKNOWN,
//! because only positive absence may authorise closing a tab.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::client::HerdrApi;
use crate::config::Config;
use crate::identity::{current_boot_id, probe_process};
use crate::reap::{plan_reap, PaneEvidence, ProcessIdentity, ReapPlan};
use crate::retention::runs_root;
use crate::session::tab_label_for;
use crate::textfmt::{quote, strict_json, strip_space};

/// Largest `meta.json` accepted. Run metadata is a few kilobytes; the bound keeps one corrupted
/// spool entry from turning a sweep into an unbounded read.
const MAX_META_BYTES: u64 = 1 << 20;

/// Parse every readable `meta.json` under the run spool, oldest run directory first.
///
/// Unreadable or malformed entries are skipped rather than failing the sweep: the spool is
/// operational output, and one truncated file must not stop the report on every other pane.
/// Directory names are timestamp-prefixed, so sorting them orders the records by run time — which
/// is what lets the LAST record for a pane be treated as its current scope.
#[must_use]
pub fn load_run_records(config: &Config) -> Vec<Value> {
    let root = runs_root(
        Path::new(&config.spool_dir),
        Path::new(&config.project_root),
    );
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut names: Vec<PathBuf> = entries
        .filter_map(|entry| Some(entry.ok()?.path()))
        .collect();
    // By the raw bytes of each name, which is the order the other edition sorts in too.
    names.sort_by(|left, right| {
        let bytes = |path: &PathBuf| path.file_name().map(|name| name.as_bytes().to_vec());
        bytes(left).cmp(&bytes(right))
    });
    let mut records = Vec::new();
    for directory in names {
        let path = directory.join("meta.json");
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if metadata.len() > MAX_META_BYTES {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        match strict_json(&text) {
            Some(mut value) if value.is_object() => {
                resolve_late_completion(&mut value, &directory);
                records.push(value);
            }
            _ => continue,
        }
    }
    records
}

/// Fill in an `exit_code` the run's own timeout could not wait for.
///
/// A run that timed out is recorded with a null exit code, which the policy reads as IN FLIGHT.
/// That is correct at the moment it is written and wrong forever afterwards: the command keeps
/// running in the pane and eventually writes `exit_code`, which is the completion signal the runner
/// was waiting for. Reading it here is what stops one timeout from making a pane permanently
/// unreapable — the safe direction, but still a leak.
fn resolve_late_completion(record: &mut Value, directory: &Path) {
    if !matches!(record.get("exit_code"), None | Some(Value::Null)) {
        return;
    }
    let Some(code) = crate::runner::read_exit_code(&directory.join("exit_code")) else {
        return;
    };
    if let Some(object) = record.as_object_mut() {
        object.insert("exit_code".to_owned(), Value::from(code));
    }
}

/// Every distinct pane id named by these records, in first-seen order.
#[must_use]
pub fn pane_ids_in(records: &[Value]) -> Vec<String> {
    let mut seen = Vec::new();
    let mut known = BTreeSet::new();
    for record in records {
        if let Some(pane_id) = record.get("pane_id").and_then(Value::as_str) {
            if !pane_id.is_empty() && known.insert(pane_id.to_owned()) {
                seen.push(pane_id.to_owned());
            }
        }
    }
    seen
}

/// Fold this pane's run records into (exit-code-recorded flags, recorded shell identity).
///
/// A record with `from_cache` true never launched anything, so it is not evidence that a pane
/// exists and is skipped — otherwise a cache hit would manufacture a phantom candidate. Two
/// different recorded shells for one pane id mean the pane was re-incarnated, and neither identity
/// may speak for the other, so the fold reports none.
#[must_use]
pub fn evidence_from_runs(
    pane_id: &str,
    records: &[Value],
) -> (Vec<bool>, Option<ProcessIdentity>) {
    let mut flags = Vec::new();
    let mut identity: Option<ProcessIdentity> = None;
    let mut conflict = false;
    for record in records {
        if record.get("pane_id").and_then(Value::as_str) != Some(pane_id) {
            continue;
        }
        if record.get("from_cache") == Some(&Value::Bool(true)) {
            continue;
        }
        let exit_code = record.get("exit_code");
        flags.push(!matches!(exit_code, None | Some(Value::Null)));
        let Some(readiness) = record.get("readiness").filter(|value| value.is_object()) else {
            continue;
        };
        let Some(shell_pid) = readiness.get("shell_pid").and_then(Value::as_i64) else {
            continue;
        };
        let candidate = ProcessIdentity {
            pid: Some(shell_pid),
            boot_id: readiness
                .get("boot_id")
                .and_then(Value::as_str)
                .filter(|boot| !strip_space(boot).is_empty())
                .map(ToOwned::to_owned),
            start_ticks: readiness.get("shell_start_ticks").and_then(Value::as_u64),
        };
        match &identity {
            None => identity = Some(candidate),
            Some(existing) if *existing != candidate => conflict = true,
            Some(_) => {}
        }
    }
    (flags, if conflict { None } else { identity })
}

/// Decide scope for one pane: `(in_scope, tab_id, tab_label, workspace_label)`.
///
/// The last record naming the pane wins: a pane id can be reused by a later incarnation, and the
/// most recent run is the only one whose labels describe the tab as it is now.
fn scope_of(
    pane_id: &str,
    records: &[Value],
    config: &Config,
) -> (bool, Option<String>, Option<String>, Option<String>) {
    let Some(latest) = records
        .iter()
        .rfind(|record| record.get("pane_id").and_then(Value::as_str) == Some(pane_id))
    else {
        return (false, None, None, None);
    };
    // Fail closed on shape: `tab` and `workspace` must be objects and every label a string (or
    // absent). Any other shape is out of scope with no labels shown, because a record that does
    // not say clearly which tab it ran in cannot authorise anything about one.
    let malformed = (false, None, None, None);
    let (Some(Value::Object(tab)), Some(Value::Object(workspace))) =
        (latest.get("tab"), latest.get("workspace"))
    else {
        return malformed;
    };
    let text = |object: &serde_json::Map<String, Value>, key: &str| match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(()),
    };
    let Some(record) = latest.as_object() else {
        return malformed;
    };
    let (Ok(tab_id), Ok(tab_label), Ok(workspace_label), Ok(agent)) = (
        text(tab, "id"),
        text(tab, "label"),
        text(workspace, "label"),
        text(record, "agent"),
    ) else {
        return malformed;
    };
    let matches_schema = match (&workspace_label, &tab_label, &agent) {
        (Some(workspace), Some(label), Some(agent)) if *workspace == config.workspace => {
            tab_label_for(config, agent).is_ok_and(|expected| expected == *label)
        }
        _ => false,
    };
    (matches_schema, tab_id, tab_label, workspace_label)
}

/// Gather one [`PaneEvidence`] per pane this tool has run a command in.
pub fn build_evidence<A: HerdrApi + ?Sized>(
    client: &A,
    config: &Config,
    records: Option<Vec<Value>>,
    proc_root: &Path,
) -> Vec<PaneEvidence> {
    let spool = records.unwrap_or_else(|| load_run_records(config));
    let boot = current_boot_id(proc_root);
    let pane_ids = pane_ids_in(&spool);
    let scopes = pane_ids
        .iter()
        .map(|pane_id| scope_of(pane_id, &spool, config))
        .collect::<Vec<_>>();

    let mut live_pane_ids: BTreeSet<String> = BTreeSet::new();
    let mut listing_error: Option<String> = None;
    if scopes.iter().any(|scope| scope.0) {
        match client.workspace_id_for_label(&config.workspace) {
            Ok(Some(workspace_id)) => match client.panes(Some(&workspace_id)) {
                Ok(panes) => live_pane_ids.extend(panes.into_iter().map(|pane| pane.pane_id)),
                // Not fatal, and NOT an empty listing: "herdr did not answer" must not be read as
                // "every pane is gone", which is the one mistake that would close the workspace.
                Err(error) => listing_error = Some(error.to_string()),
            },
            // No workspace by that label means we never got a listing at all. Saying nothing here
            // would leave every pane GONE, which tells an operator the tabs are already closed —
            // the opposite of what happened.
            Ok(None) => {
                listing_error = Some(format!(
                    "herdr has no workspace labelled {}",
                    quote(&config.workspace)
                ));
            }
            Err(error) => listing_error = Some(error.to_string()),
        }
    }

    let mut evidence = Vec::new();
    for (pane_id, (in_scope, tab_id, tab_label, workspace_label)) in
        pane_ids.into_iter().zip(scopes)
    {
        let (flags, recorded) = evidence_from_runs(&pane_id, &spool);
        let known = live_pane_ids.contains(&pane_id);
        let mut live = None;
        let mut error = listing_error.clone();
        if in_scope && error.is_none() && known {
            let (identity, failure) = live_shell(client, &pane_id, boot.as_deref(), proc_root);
            live = identity;
            error = failure;
        }
        evidence.push(PaneEvidence {
            pane_id,
            tab_id,
            tab_label,
            workspace_label,
            in_scope,
            run_exit_codes_recorded: flags,
            recorded_shell: recorded,
            live_shell: live,
            current_boot_id: boot.clone(),
            pane_known_to_herdr: known,
            evidence_error: error,
        });
    }
    evidence
}

/// Bind the pane's CURRENT shell, or say why we could not.
///
/// `None` for the identity means the shell is positively gone — the only reading the policy may
/// turn into a STALE verdict. Everything inconclusive comes back as an error string instead.
fn live_shell<A: HerdrApi + ?Sized>(
    client: &A,
    pane_id: &str,
    boot: Option<&str>,
    proc_root: &Path,
) -> (Option<ProcessIdentity>, Option<String>) {
    let info = match client.process_info(pane_id) {
        Ok(info) => info,
        Err(error) => {
            return (
                None,
                Some(format!("cannot read process info for {pane_id}: {error}")),
            )
        }
    };
    let probe = probe_process(info.shell_pid, proc_root);
    if let Some(error) = probe.error {
        return (None, Some(error));
    }
    if probe.gone {
        return (None, None);
    }
    (
        Some(ProcessIdentity {
            pid: Some(info.shell_pid),
            boot_id: boot.map(ToOwned::to_owned),
            start_ticks: probe.start_ticks,
        }),
        None,
    )
}

/// What holds `max_panes`: herdr's live pane list, split by whether a run record names the pane.
///
/// The cap counts every pane herdr lists in the workspace, while `reap` can judge only the panes a
/// surviving run record names. `without_record` is the difference: panes holding a share of the
/// cap that no verdict covers and only a human can close. All three counts are `None` when the
/// listing failed, because a failed listing is not an empty workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occupancy {
    /// Panes herdr lists in the workspace: the number `max_panes` is compared against.
    pub live_panes: Option<usize>,
    /// Of those, the panes a surviving run record names, which `reap` can judge.
    pub with_record: Option<usize>,
    /// Why the listing failed, when it did.
    pub listing_error: Option<String>,
}

impl Occupancy {
    /// Live panes no run record names: they hold a share of the cap and only a human can close them.
    #[must_use]
    pub fn without_record(&self) -> Option<usize> {
        Some(self.live_panes? - self.with_record?)
    }
}

/// List the workspace the way the pane cap does, and count the panes no run record names.
pub fn measure_occupancy<A: HerdrApi + ?Sized>(
    client: &A,
    config: &Config,
    records: &[Value],
) -> Occupancy {
    let failed = |error: String| Occupancy {
        live_panes: None,
        with_record: None,
        listing_error: Some(error),
    };
    let panes = match client.workspace_id_for_label(&config.workspace) {
        Ok(Some(workspace_id)) => match client.panes(Some(&workspace_id)) {
            Ok(panes) => panes,
            Err(error) => return failed(error.to_string()),
        },
        Ok(None) => {
            return failed(format!(
                "herdr has no workspace labelled {}",
                quote(&config.workspace)
            ))
        }
        Err(error) => return failed(error.to_string()),
    };
    let recorded: BTreeSet<String> = pane_ids_in(records).into_iter().collect();
    Occupancy {
        live_panes: Some(panes.len()),
        with_record: Some(
            panes
                .iter()
                .filter(|pane| recorded.contains(&pane.pane_id))
                .count(),
        ),
        listing_error: None,
    }
}

/// Gather evidence and run the reaping policy over it. Decides only; closes nothing.
pub fn sweep<A: HerdrApi + ?Sized>(client: &A, config: &Config, proc_root: &Path) -> ReapPlan {
    plan_reap(&build_evidence(client, config, None, proc_root))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use crate::client::{Pane, ProcessInfo};
    use crate::error::{HerdrRunError, Result};
    use crate::reap::Verdict;

    use super::*;

    /// Reap scope and recorded evidence shared with the other edition.
    #[test]
    fn golden_scope_and_evidence() {
        let document: Value = serde_json::from_str(include_str!("../testdata/record_cases.json"))
            .expect("golden record cases");
        let settings = &document["scope_config"];
        let config = Config {
            workspace: settings["workspace"].as_str().unwrap().to_owned(),
            tab_name: settings["tab_name"].as_str().unwrap().to_owned(),
            project_root: "/p".to_owned(),
            ..Config::default()
        };
        for row in document["scope"].as_array().expect("scope rows") {
            let records = row[1].as_array().unwrap();
            let (in_scope, tab_id, tab_label, workspace_label) =
                scope_of(row[0].as_str().unwrap(), records, &config);
            assert_eq!(
                json!([in_scope, tab_id, tab_label, workspace_label]),
                row[2],
                "{row:?}"
            );
        }
        for row in document["evidence"].as_array().expect("evidence rows") {
            let (flags, identity) =
                evidence_from_runs(row[0].as_str().unwrap(), row[1].as_array().unwrap());
            assert_eq!(json!(flags), row[2], "{row:?}");
            let shown = identity.map_or(Value::Null, |identity| {
                json!([identity.pid, identity.boot_id, identity.start_ticks])
            });
            assert_eq!(shown, row[3], "{row:?}");
        }
    }

    const BOOT: &str = "3f2b1c8e-0000-4000-8000-000000000001";

    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    /// One live workspace holding chosen panes, each with a chosen shell pid.
    struct SweepFake {
        panes: Vec<Pane>,
        shell_pids: BTreeMap<String, i64>,
        workspace_queries: AtomicUsize,
        fail_pane_list: bool,
        /// When true, `process_info` fails the way a busy or restarting server does. A control call
        /// that did not answer says nothing about whether the pane's shell is alive, so the sweep
        /// must reach UNKNOWN and not "the shell is gone".
        fail_process_info: bool,
        /// When false, `workspace_id_for_label` resolves nothing — the workspace was renamed or
        /// deleted out from under us.
        workspace_exists: bool,
    }

    impl SweepFake {
        fn with_pane(pane_id: &str, shell_pid: i64) -> Self {
            Self {
                panes: vec![Pane {
                    pane_id: pane_id.to_owned(),
                    tab_id: "w1:t1".to_owned(),
                    workspace_id: "w1".to_owned(),
                }],
                shell_pids: BTreeMap::from([(pane_id.to_owned(), shell_pid)]),
                workspace_queries: AtomicUsize::new(0),
                fail_pane_list: false,
                fail_process_info: false,
                workspace_exists: true,
            }
        }
    }

    impl HerdrApi for SweepFake {
        fn ensure_server(&self) -> Result<bool> {
            Ok(false)
        }

        fn server_running(&self) -> bool {
            true
        }

        fn workspace_id_for_label(&self, _label: &str) -> Result<Option<String>> {
            self.workspace_queries.fetch_add(1, Ordering::Relaxed);
            Ok(self.workspace_exists.then(|| "w1".to_owned()))
        }

        fn create_workspace(&self, _label: &str, _cwd: &str) -> Result<(String, String, String)> {
            unreachable!()
        }

        fn tab_id_for_label(&self, _workspace_id: &str, _label: &str) -> Result<Option<String>> {
            Ok(None)
        }

        fn create_tab(&self, _workspace_id: &str, _label: &str, _cwd: &str) -> Result<String> {
            unreachable!()
        }

        fn rename_tab(&self, _tab_id: &str, _label: &str) -> Result<()> {
            Ok(())
        }

        fn panes(&self, _workspace_id: Option<&str>) -> Result<Vec<Pane>> {
            if self.fail_pane_list {
                return Err(HerdrRunError::unavailable(
                    "pane list: herdr is not answering",
                ));
            }
            Ok(self.panes.clone())
        }

        fn pane_exists(&self, pane_id: &str) -> bool {
            self.panes.iter().any(|pane| pane.pane_id == pane_id)
        }

        fn process_info(&self, pane_id: &str) -> Result<ProcessInfo> {
            if self.fail_process_info {
                return Err(HerdrRunError::unavailable(
                    "pane process-info: herdr is not answering",
                ));
            }
            let shell_pid = *self.shell_pids.get(pane_id).unwrap_or(&100);
            Ok(ProcessInfo {
                pane_id: pane_id.to_owned(),
                shell_pid,
                foreground_pgid: shell_pid,
                foreground: vec![(shell_pid, "bash".to_owned(), "/bin/bash".to_owned())],
            })
        }

        fn read(&self, _pane_id: &str, _source: &str, _lines: Option<usize>) -> Result<String> {
            unreachable!()
        }

        fn run(&self, _pane_id: &str, _command: &str) -> Result<()> {
            unreachable!()
        }
    }

    fn temporary_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "herdr-run-sweep-{name}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temporary root");
        path
    }

    fn config(root: &Path) -> Config {
        Config {
            project_root: root.to_string_lossy().into_owned(),
            spool_dir: "spool".to_owned(),
            ..Config::default()
        }
    }

    fn record(pane_id: &str, exit_code: Value, workspace: &str, agent: &str) -> Value {
        json!({
            "pane_id": pane_id,
            "agent": agent,
            "exit_code": exit_code,
            "workspace": {"label": workspace, "id": "w1"},
            "tab": {"label": agent, "id": "w1:t1"},
            "readiness": {
                "shell_pid": 4242,
                "boot_id": BOOT,
                "shell_start_ticks": 900,
            },
        })
    }

    fn write_spool(root: &Path, records: &[Value]) {
        let runs = root.join("spool").join("runs");
        for (index, value) in records.iter().enumerate() {
            let directory = runs.join(format!("20260819T00000{index}-agent-{index}"));
            fs::create_dir_all(&directory).expect("run directory");
            fs::write(
                directory.join("meta.json"),
                serde_json::to_vec(value).expect("encode"),
            )
            .expect("write meta");
        }
    }

    fn proc_with(root: &Path, pid: Option<i64>, ticks: u64) -> PathBuf {
        let proc = root.join("proc");
        fs::create_dir_all(proc.join("sys/kernel/random")).expect("proc tree");
        fs::write(proc.join(BOOT_ID_PATH_TEST), format!("{BOOT}\n")).expect("boot id");
        if let Some(pid) = pid {
            fs::create_dir_all(proc.join(pid.to_string())).expect("pid directory");
            let padding = vec!["0"; 18].join(" ");
            fs::write(
                proc.join(pid.to_string()).join("stat"),
                format!("{pid} (bash) S {padding} {ticks}\n"),
            )
            .expect("stat");
        }
        proc
    }

    const BOOT_ID_PATH_TEST: &str = crate::identity::BOOT_ID_PATH;

    #[test]
    fn load_run_records_skips_a_corrupt_entry_without_losing_the_rest() {
        let root = temporary_root("corrupt");
        write_spool(
            &root,
            &[
                record("w1:p1", json!(0), "agent-cmds", "kvm"),
                record("w1:p2", json!(0), "agent-cmds", "kvm"),
            ],
        );
        let broken = root.join("spool/runs/20260819T999999-broken");
        fs::create_dir_all(&broken).expect("broken directory");
        fs::write(broken.join("meta.json"), "{not json").expect("write broken");

        let records = load_run_records(&config(&root));
        assert_eq!(pane_ids_in(&records), ["w1:p1", "w1:p2"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_husk_pane_whose_shell_died_is_reaped() {
        // POSITIVE CONTROL. Herdr still lists the tab; the shell that owned it no longer exists.
        let root = temporary_root("husk");
        write_spool(&root, &[record("w1:p1", json!(0), "agent-cmds", "kvm")]);
        let proc = proc_with(&root, None, 0);
        let plan = sweep(&SweepFake::with_pane("w1:p1", 4242), &config(&root), &proc);
        assert_eq!(plan.counts()["STALE"], 1);
        assert_eq!(plan.reapable()[0].pane_id, "w1:p1");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_live_shell_is_spared() {
        let root = temporary_root("live");
        write_spool(&root, &[record("w1:p1", json!(0), "agent-cmds", "kvm")]);
        let proc = proc_with(&root, Some(4242), 900);
        let plan = sweep(&SweepFake::with_pane("w1:p1", 4242), &config(&root), &proc);
        assert_eq!(plan.counts()["SHELL_ALIVE"], 1);
        assert!(plan.reapable().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_unfinished_run_beats_a_dead_shell() {
        let root = temporary_root("thinking");
        write_spool(
            &root,
            &[
                record("w1:p1", json!(0), "agent-cmds", "kvm"),
                record("w1:p1", Value::Null, "agent-cmds", "kvm"),
            ],
        );
        let proc = proc_with(&root, None, 0);
        let plan = sweep(&SweepFake::with_pane("w1:p1", 4242), &config(&root), &proc);
        assert_eq!(plan.counts()["IN_FLIGHT"], 1);
        assert_eq!(plan.counts()["STALE"], 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_unreadable_proc_entry_is_unknown_not_stale() {
        let root = temporary_root("unreadable");
        write_spool(&root, &[record("w1:p1", json!(0), "agent-cmds", "kvm")]);
        let proc = proc_with(&root, None, 0);
        fs::create_dir_all(proc.join("4242")).expect("pid directory");
        fs::write(proc.join("4242").join("stat"), "truncated\n").expect("stat");
        let plan = sweep(&SweepFake::with_pane("w1:p1", 4242), &config(&root), &proc);
        assert_eq!(plan.counts()["UNKNOWN"], 1);
        assert_eq!(plan.counts()["STALE"], 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_proc_entry_that_cannot_be_opened_is_unknown_not_stale() {
        // Same verdict as the unparseable case, reached through the READ-error arm instead. There
        // is no /proc/4242 sibling, so the only thing standing between this pane and a STALE
        // verdict is the refusal to read an open failure as a death.
        let root = temporary_root("unopenable");
        write_spool(&root, &[record("w1:p1", json!(0), "agent-cmds", "kvm")]);
        let proc = proc_with(&root, None, 0);
        fs::create_dir_all(proc.join("4242").join("stat")).expect("stat directory");
        let plan = sweep(&SweepFake::with_pane("w1:p1", 4242), &config(&root), &proc);
        assert_eq!(plan.counts()["UNKNOWN"], 1);
        assert_eq!(plan.counts()["STALE"], 0);
        assert!(
            plan.declined()[0].reason.contains("cannot read"),
            "{}",
            plan.declined()[0].reason
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_oversized_proc_entry_is_unknown_not_stale() {
        let root = temporary_root("oversized");
        write_spool(&root, &[record("w1:p1", json!(0), "agent-cmds", "kvm")]);
        let proc = proc_with(&root, None, 0);
        fs::create_dir_all(proc.join("4242")).expect("pid directory");
        let padding = "0 ".repeat(40_000);
        fs::write(
            proc.join("4242").join("stat"),
            format!("4242 (bash) S {padding}\n"),
        )
        .expect("stat");
        let plan = sweep(&SweepFake::with_pane("w1:p1", 4242), &config(&root), &proc);
        assert_eq!(plan.counts()["UNKNOWN"], 1);
        assert_eq!(plan.counts()["STALE"], 0);
        assert!(
            plan.declined()[0].reason.contains("implausibly long"),
            "{}",
            plan.declined()[0].reason
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failing_process_info_call_is_unknown_not_stale() {
        // A control call that did not answer is not a report that the shell died. The pane is
        // listed and /proc holds no such pid, so every other signal points at STALE; only the
        // refusal to treat a failed `pane process-info` as evidence keeps the tab open.
        let root = temporary_root("process-info-failure");
        write_spool(&root, &[record("w1:p1", json!(0), "agent-cmds", "kvm")]);
        let proc = proc_with(&root, None, 0);
        let mut fake = SweepFake::with_pane("w1:p1", 4242);
        fake.fail_process_info = true;
        let plan = sweep(&fake, &config(&root), &proc);
        assert_eq!(plan.counts()["STALE"], 0);
        assert_eq!(plan.counts()["UNKNOWN"], 1);
        assert!(
            plan.declined()[0]
                .reason
                .contains("cannot read process info for w1:p1"),
            "{}",
            plan.declined()[0].reason
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_workspace_herdr_does_not_know_is_reported_as_a_missing_listing() {
        // No workspace by that label means no listing was obtained, not that the tabs are gone.
        let root = temporary_root("workspace-renamed");
        write_spool(&root, &[record("w1:p1", json!(0), "agent-cmds", "kvm")]);
        let proc = proc_with(&root, None, 0);
        let mut fake = SweepFake::with_pane("w1:p1", 4242);
        fake.workspace_exists = false;
        let plan = sweep(&fake, &config(&root), &proc);
        assert_eq!(plan.counts()["STALE"], 0);
        assert_eq!(plan.counts()["UNKNOWN"], 1);
        let reason = &plan.declined()[0].reason;
        assert!(reason.contains("no workspace labelled"), "{reason}");
        assert_eq!(plan.counts()["GONE"], 0);
        assert!(!reason.contains("already closed"), "{reason}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_run_that_finished_after_the_timeout_stops_looking_in_flight() {
        // The completion signal outlives the process that was waiting for it. A timed-out run is
        // recorded with a null exit code, which is true when it is written; the command then keeps
        // running and writes `exit_code` into the same directory. Reading that back is what stops
        // one timeout from making the pane permanently unreapable.
        let root = temporary_root("late-completion");
        write_spool(&root, &[record("w1:p1", Value::Null, "agent-cmds", "kvm")]);
        fs::write(
            root.join("spool/runs/20260819T000000-agent-0/exit_code"),
            "0\n",
        )
        .expect("plant completion signal");
        let proc = proc_with(&root, None, 0);
        let plan = sweep(&SweepFake::with_pane("w1:p1", 4242), &config(&root), &proc);
        assert_eq!(plan.counts()["IN_FLIGHT"], 0);
        assert_eq!(plan.counts()["STALE"], 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_run_still_waiting_on_its_exit_code_stays_in_flight() {
        // Negative control for the case above: created but empty, as during a torn write.
        let root = temporary_root("torn-completion");
        write_spool(&root, &[record("w1:p1", Value::Null, "agent-cmds", "kvm")]);
        fs::write(
            root.join("spool/runs/20260819T000000-agent-0/exit_code"),
            "",
        )
        .expect("plant torn signal");
        let proc = proc_with(&root, None, 0);
        let plan = sweep(&SweepFake::with_pane("w1:p1", 4242), &config(&root), &proc);
        assert_eq!(plan.counts()["IN_FLIGHT"], 1);
        assert_eq!(plan.counts()["STALE"], 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn load_run_records_skips_an_absurdly_large_entry() {
        let root = temporary_root("huge-meta");
        write_spool(
            &root,
            &[
                record("w1:p1", json!(0), "agent-cmds", "kvm"),
                record("w1:p2", json!(0), "agent-cmds", "kvm"),
            ],
        );
        let huge = root.join("spool/runs/20260819T000000a-huge");
        fs::create_dir_all(&huge).expect("huge directory");
        let padding = "x".repeat(1 << 21);
        fs::write(
            huge.join("meta.json"),
            serde_json::to_vec(&json!({"pane_id": "w1:pHUGE", "padding": padding}))
                .expect("encode"),
        )
        .expect("write huge");
        let records = load_run_records(&config(&root));
        assert_eq!(
            pane_ids_in(&records),
            ["w1:p1", "w1:p2"],
            "an unbounded read must not be attempted"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn records_arrive_in_run_order_whatever_the_directory_listing_says() {
        // `names.sort()` is what makes "the LAST record wins" mean "the most recent run". The run
        // directories are created newest-first here, so a listing taken in creation order would
        // hand the sweep the records backwards and make the oldest labels authoritative.
        let root = temporary_root("ordering");
        let runs = root.join("spool").join("runs");
        for (stamp, pane) in [("20260819T000003", "w1:p3"), ("20260819T000001", "w1:p1")] {
            let directory = runs.join(format!("{stamp}-agent"));
            fs::create_dir_all(&directory).expect("run directory");
            fs::write(
                directory.join("meta.json"),
                serde_json::to_vec(&record(pane, json!(0), "agent-cmds", "kvm")).expect("encode"),
            )
            .expect("write meta");
        }
        assert_eq!(
            pane_ids_in(&load_run_records(&config(&root))),
            ["w1:p1", "w1:p3"]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scope_comes_from_the_latest_record_not_the_first() {
        // A pane retargeted into another workspace must lose the authority its first run had.
        // Keeping the FIRST record is the unsafe direction: the tab would stay in scope on labels
        // that no longer describe it, and this planted population would then be reaped.
        let root = temporary_root("latest-scope");
        write_spool(
            &root,
            &[
                record("w1:p1", json!(0), "agent-cmds", "kvm"),
                record("w1:p1", json!(0), "someone-elses", "kvm"),
            ],
        );
        let proc = proc_with(&root, None, 0); // the shell is gone; only scope can spare this pane
        let plan = sweep(&SweepFake::with_pane("w1:p1", 4242), &config(&root), &proc);
        assert_eq!(plan.counts()["OUT_OF_SCOPE"], 1);
        assert_eq!(plan.counts()["STALE"], 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retention_takes_the_oldest_leaked_tabs_out_of_the_candidate_set() {
        // Pins the known gap so it cannot become news later. The reaper's candidates are the panes
        // named by SURVIVING run records, and herdr-run's own retention deletes those records. A
        // tab whose agent last ran longer ago than the window is invisible here while still
        // counting against `max_panes`; `herdr-run reap` prints the window for exactly this reason.
        let root = temporary_root("retention-window");
        write_spool(&root, &[record("w1:pLEAK", json!(0), "agent-cmds", "kvm")]);
        let runs = root.join("spool").join("runs");
        let exit_code = runs.join("20260819T000000-agent-0").join("exit_code");
        fs::write(&exit_code, "0\n").expect("plant completion signal");
        let ancient =
            std::time::SystemTime::now() - std::time::Duration::from_secs(6 * 24 * 60 * 60);
        fs::File::open(&exit_code)
            .expect("open completion marker")
            .set_times(fs::FileTimes::new().set_modified(ancient))
            .expect("backdate completion marker");

        assert_eq!(pane_ids_in(&load_run_records(&config(&root))), ["w1:pLEAK"]);
        let result = crate::retention::prune_runs(&runs, 4);
        assert_eq!(result.removed.len(), 1);
        assert!(pane_ids_in(&load_run_records(&config(&root))).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_tab_retargeted_out_of_the_schema_is_out_of_scope() {
        // Scope is recomputed from the CURRENT config, not read back from the record.
        let root = temporary_root("retargeted");
        write_spool(&root, &[record("w1:p1", json!(0), "agent-cmds", "kvm")]);
        let proc = proc_with(&root, None, 0);
        let mut cfg = config(&root);
        cfg.tab_name = "{project}-{agent}".to_owned();
        let plan = sweep(&SweepFake::with_pane("w1:p1", 4242), &cfg, &proc);
        assert_eq!(plan.counts()["OUT_OF_SCOPE"], 1);
        assert_eq!(plan.counts()["STALE"], 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_pane_in_another_workspace_is_out_of_scope() {
        let root = temporary_root("other-workspace");
        write_spool(&root, &[record("w1:p1", json!(0), "someone-elses", "kvm")]);
        let proc = proc_with(&root, None, 0);
        let fake = SweepFake::with_pane("w1:p1", 4242);
        let plan = sweep(&fake, &config(&root), &proc);
        assert_eq!(fake.workspace_queries.load(Ordering::Relaxed), 0);
        assert_eq!(plan.counts()["OUT_OF_SCOPE"], 1);
        assert!(plan.reapable().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_pane_missing_from_herdrs_listing_is_gone() {
        let root = temporary_root("delisted");
        write_spool(
            &root,
            &[
                record("w1:p1", json!(0), "agent-cmds", "kvm"),
                record("w1:pGONE", json!(0), "agent-cmds", "kvm"),
            ],
        );
        let proc = proc_with(&root, Some(4242), 900);
        let plan = sweep(&SweepFake::with_pane("w1:p1", 4242), &config(&root), &proc);
        let verdicts: BTreeMap<&str, Verdict> = plan
            .decisions
            .iter()
            .map(|decision| (decision.pane_id.as_str(), decision.verdict))
            .collect();
        assert_eq!(verdicts["w1:pGONE"], Verdict::Gone);
        assert_eq!(verdicts["w1:p1"], Verdict::ShellAlive);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_unanswering_herdr_reaps_nothing() {
        let root = temporary_root("unanswering");
        write_spool(&root, &[record("w1:p1", json!(0), "agent-cmds", "kvm")]);
        let proc = proc_with(&root, None, 0);
        let mut fake = SweepFake::with_pane("w1:p1", 4242);
        fake.fail_pane_list = true;
        let plan = sweep(&fake, &config(&root), &proc);
        assert_eq!(fake.workspace_queries.load(Ordering::Relaxed), 1);
        assert_eq!(plan.counts()["STALE"], 0);
        assert_eq!(plan.counts()["UNKNOWN"], 1);
        // And it must be UNKNOWN saying the SERVER did not answer, never GONE: GONE tells an
        // operator the tabs are already closed -- which is the opposite of what happened. Anchored
        // on the two strings
        // PRODUCTION owns: the sweep's "evidence unavailable" wrapper and the control call's own
        // purpose prefix, which the client puts in front of every failure it raises.
        let reason = &plan.declined()[0].reason;
        assert!(reason.starts_with("evidence unavailable: "), "{reason}");
        assert!(reason.contains("pane list"), "{reason}");
        assert_eq!(plan.counts()["GONE"], 0);
        assert!(!reason.contains("already closed"), "{reason}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_empty_spool_reports_zero_of_everything() {
        let root = temporary_root("empty");
        let proc = proc_with(&root, None, 0);
        let fake = SweepFake::with_pane("w1:p1", 4242);
        let plan = sweep(&fake, &config(&root), &proc);
        assert_eq!(fake.workspace_queries.load(Ordering::Relaxed), 0);
        let counts = plan.counts();
        assert_eq!(counts["considered"], 0);
        for verdict in Verdict::all() {
            assert_eq!(counts[verdict.as_str()], 0);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn evidence_carries_the_recorded_identity_the_policy_needs() {
        let root = temporary_root("identity");
        write_spool(&root, &[record("w1:p1", json!(0), "agent-cmds", "kvm")]);
        let proc = proc_with(&root, Some(4242), 900);
        let evidence = build_evidence(
            &SweepFake::with_pane("w1:p1", 4242),
            &config(&root),
            None,
            &proc,
        );
        assert_eq!(evidence.len(), 1);
        let recorded = evidence[0].recorded_shell.as_ref().expect("recorded shell");
        assert!(
            recorded.is_bound(),
            "a bare pid can never authorise anything"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_cache_hit_does_not_manufacture_a_candidate() {
        let mut cached = record("w1:p1", json!(0), "agent-cmds", "kvm");
        cached["from_cache"] = json!(true);
        let (flags, identity) = evidence_from_runs("w1:p1", &[cached]);
        assert!(flags.is_empty());
        assert!(identity.is_none());
    }

    #[test]
    fn conflicting_pane_shell_incarnations_forfeit_the_identity() {
        let mut first = record("w1:p1", json!(0), "agent-cmds", "kvm");
        first["readiness"]["shell_pid"] = json!(100);
        let mut second = record("w1:p1", json!(0), "agent-cmds", "kvm");
        second["readiness"]["shell_pid"] = json!(200);
        let (flags, identity) = evidence_from_runs("w1:p1", &[first, second]);
        assert_eq!(flags, [true, true]);
        assert!(
            identity.is_none(),
            "a recycled pane id must not inherit the first shell's authority"
        );
    }

    fn occupancy_fake() -> SweepFake {
        let mut fake = SweepFake::with_pane("w1:p1", 100);
        for pane_id in ["w1:p2", "w1:p3"] {
            fake.panes.push(Pane {
                pane_id: pane_id.to_owned(),
                tab_id: "w1:t2".to_owned(),
                workspace_id: "w1".to_owned(),
            });
        }
        fake
    }

    #[test]
    fn occupancy_counts_live_panes_no_record_names() {
        let root = temporary_root("occupancy");
        let records = [
            record("w1:p1", json!(0), "agent-cmds", "kvm"),
            // A record whose pane herdr no longer lists is not occupancy.
            record("w1:p9", json!(0), "agent-cmds", "old"),
        ];
        let occupancy = measure_occupancy(&occupancy_fake(), &config(&root), &records);
        assert_eq!(occupancy.live_panes, Some(3));
        assert_eq!(occupancy.with_record, Some(1));
        assert_eq!(occupancy.without_record(), Some(2));
        assert_eq!(occupancy.listing_error, None);
    }

    #[test]
    fn occupancy_of_a_failed_listing_is_unknown_not_zero() {
        let root = temporary_root("occupancy-fail");
        let mut fake = occupancy_fake();
        fake.fail_pane_list = true;
        let occupancy = measure_occupancy(&fake, &config(&root), &[]);
        assert_eq!(occupancy.live_panes, None);
        assert_eq!(occupancy.without_record(), None);
        assert!(occupancy
            .listing_error
            .as_deref()
            .is_some_and(|error| error.contains("not answering")));
    }

    #[test]
    fn occupancy_of_a_missing_workspace_names_it() {
        let root = temporary_root("occupancy-missing");
        let mut fake = occupancy_fake();
        fake.workspace_exists = false;
        let occupancy = measure_occupancy(&fake, &config(&root), &[]);
        assert_eq!(occupancy.with_record, None);
        assert_eq!(
            occupancy.listing_error.as_deref(),
            Some("herdr has no workspace labelled \"agent-cmds\"")
        );
    }
}
