//! Idempotent resolution of the Herdr server/workspace/tab/pane chain.
//!
//! The on-disk cache is only an optimization.  Every cached ID is checked against live Herdr
//! state, including the workspace and tab labels, before it can select a pane.  Cache updates use
//! an inter-process advisory lock and a unique temporary-file rename so concurrent callers cannot
//! tear the document or silently discard each other's entries.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::client::HerdrApi;
use crate::config::{render_tab_name, Config};
use crate::error::{HerdrRunError, Result};
use crate::evict;
use crate::state::{open_lock_file, session_lock_path};
use crate::textfmt::{json_pretty, quote, strict_json};

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A fully resolved destination pane and the session objects created to obtain it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    /// Herdr workspace identifier.
    pub workspace_id: String,
    /// Herdr tab identifier.
    pub tab_id: String,
    /// Herdr pane identifier.
    pub pane_id: String,
    /// Expected and live-validated workspace label.
    pub workspace_label: String,
    /// Expected and live-validated tab label.
    pub tab_label: String,
    /// Session levels created by this resolution, in creation order.
    pub created: Vec<String>,
    /// Whether the result came from a revalidated cache entry.
    pub from_cache: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CacheRecord {
    workspace_id: String,
    tab_id: String,
    pane_id: String,
    #[serde(default)]
    workspace_label: Option<String>,
    #[serde(default)]
    tab_label: Option<String>,
}

/// Render the configured restricted tab-label template for `agent`.
///
/// Only literal text, doubled opening or closing braces, and the exact `agent` and `project`
/// placeholders are accepted. Attribute access, conversions, format specifications, positional
/// fields, and unmatched braces are configuration errors.
pub fn tab_label_for(config: &Config, agent: &str) -> Result<String> {
    let project = Path::new(&config.project_root)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("project");
    render_tab_name(&config.tab_name, agent, project)
}

/// Return the session-cache path for `config`.
#[must_use]
pub fn cache_path(config: &Config) -> PathBuf {
    Path::new(&config.project_root)
        .join(&config.spool_dir)
        .join("session-cache.json")
}

/// Ensure and resolve the destination pane for `agent`.
///
/// When `use_cache` is true, a cache hit is used only after all IDs and labels agree with the live
/// session.  Cache read or write failures never prevent label-based resolution.
pub fn resolve_target<A: HerdrApi + ?Sized>(
    client: &A,
    config: &Config,
    agent: &str,
    use_cache: bool,
) -> Result<Target> {
    let lock_path = session_lock_path()?;
    let lock = open_lock_file(&lock_path)?;
    FileExt::lock_exclusive(&lock).map_err(|error| {
        HerdrRunError::unavailable(format!(
            "cannot lock Herdr session resolution at {}: {error}",
            lock_path.display()
        ))
    })?;
    resolve_target_locked(client, config, agent, use_cache)
}

fn resolve_target_locked<A: HerdrApi + ?Sized>(
    client: &A,
    config: &Config,
    agent: &str,
    use_cache: bool,
) -> Result<Target> {
    let tab_label = tab_label_for(config, agent)?;
    let cwd = resolved_cwd(config)?;
    let cwd = cwd.to_str().ok_or_else(|| {
        HerdrRunError::config(format!(
            "working directory is not valid UTF-8: {}",
            cwd.display()
        ))
    })?;
    let key = format!("{}\0{tab_label}", config.workspace);
    let path = cache_path(config);
    let mut created = Vec::new();

    if client.ensure_server()? {
        created.push("server".to_owned());
    }

    if use_cache && created.is_empty() {
        if let Some(cached) = load_cache(&path, &key) {
            if cache_still_valid(client, &cached, &config.workspace, &tab_label)? {
                return Ok(Target {
                    workspace_id: cached.workspace_id,
                    tab_id: cached.tab_id,
                    pane_id: cached.pane_id,
                    workspace_label: config.workspace.clone(),
                    tab_label,
                    created,
                    from_cache: true,
                });
            }
        }
    }

    let (workspace_id, tab_id) = match client.workspace_id_for_label(&config.workspace)? {
        Some(workspace_id) => {
            let tab_id = match client.tab_id_for_label(&workspace_id, &tab_label)? {
                Some(tab_id) => tab_id,
                None => {
                    enforce_pane_cap(client, config, &workspace_id, agent, Path::new("/proc"))?;
                    let tab_id = client.create_tab(&workspace_id, &tab_label, cwd)?;
                    created.push("tab".to_owned());
                    tab_id
                }
            };
            (workspace_id, tab_id)
        }
        None => {
            // A new workspace already has one root tab. Rename it instead of leaving a redundant
            // default tab and adding another one.
            let (workspace_id, root_tab_id, _root_pane_id) =
                client.create_workspace(&config.workspace, cwd)?;
            client.rename_tab(&root_tab_id, &tab_label)?;
            created.extend(["workspace".to_owned(), "tab".to_owned()]);
            (workspace_id, root_tab_id)
        }
    };

    let pane_id = pane_of_tab(client, &workspace_id, &tab_id)?;
    let target = Target {
        workspace_id,
        tab_id,
        pane_id,
        workspace_label: config.workspace.clone(),
        tab_label,
        created,
        from_cache: false,
    };
    // The cache is deliberately non-authoritative. A read-only or damaged spool must not turn a
    // usable live Herdr session into a failed command.
    let _ = store_cache(&path, &key, &target);
    Ok(target)
}

/// Make room for ANOTHER tab once the workspace is already at `max_panes`, or refuse.
///
/// Agents are coined continuously and every one that runs a command leaves a tab behind, so the
/// command workspace grows without bound until the Herdr server itself becomes the bottleneck
/// (measured: 260 panes, >1000% CPU, every control call timing out). At the ceiling the
/// least-recently-used tab whose shell is provably idle is closed and the new tab takes its place
/// (see [`crate::evict`]); a tab running a command is never closed. Only when no tab is idle does
/// this refuse, with one legible message naming the remedy.
///
/// Checked ONLY on the create path. An agent whose tab already exists must never be locked out of
/// it -- a cap that can break work in progress is a cap that gets switched off -- and a cap that
/// refused an existing tab would also make the failure arrive at a random later moment rather than
/// when a new tab is actually being added.
fn enforce_pane_cap<A: HerdrApi + ?Sized>(
    client: &A,
    config: &Config,
    workspace_id: &str,
    agent: &str,
    proc_root: &Path,
) -> Result<()> {
    if config.max_panes == 0 {
        return Ok(());
    }
    let mut existing = client.panes(Some(workspace_id))?.len() as u64;
    // Bounded: a close that the listing never reflects must not turn into closing every tab.
    let mut budget = existing.saturating_sub(config.max_panes) + 1;
    let mut skipped = Vec::new();
    while existing >= config.max_panes && budget > 0 {
        budget -= 1;
        match evict::evict_one(client, config, workspace_id, agent, proc_root)? {
            Ok(_) => existing = client.panes(Some(workspace_id))?.len() as u64,
            Err(reasons) => {
                skipped = reasons;
                break;
            }
        }
    }
    if existing < config.max_panes {
        return Ok(());
    }
    let source = config
        .source_path
        .as_deref()
        .unwrap_or("your .herdr-run.yaml");
    let mut message = format!(
        "workspace {} already holds {existing} pane(s) and max_panes is {}; refusing to open a tab for this agent. Run 'herdr-run reap' to see which tabs are provably finished and can be closed, or raise max_panes in {source} (0 disables the cap).",
        quote(&config.workspace),
        config.max_panes
    );
    if !skipped.is_empty() {
        message.push(' ');
        message.push_str(&evict::describe_skipped(&skipped));
    }
    Err(HerdrRunError::unavailable(message))
}

/// The directory a new workspace or tab starts in: the configured `cwd`, else the project root.
///
/// An absolute path is passed through exactly as written, and the pane's `cd` resolves it; a
/// relative one is resolved lexically against the project root.
fn resolved_cwd(config: &Config) -> Result<PathBuf> {
    let root = Path::new(&config.project_root);
    let cwd = match config.cwd.as_deref() {
        Some(cwd) if !cwd.is_empty() => Path::new(cwd),
        _ => root,
    };
    if cwd.is_absolute() {
        return Ok(cwd.to_path_buf());
    }
    let joined = root.join(cwd);
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.parent().is_some() {
                    normalized.pop();
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn pane_of_tab<A: HerdrApi + ?Sized>(
    client: &A,
    workspace_id: &str,
    tab_id: &str,
) -> Result<String> {
    let panes = client
        .panes(Some(workspace_id))?
        .into_iter()
        .filter(|pane| pane.tab_id == tab_id)
        .collect::<Vec<_>>();
    match panes.as_slice() {
        [] => Err(HerdrRunError::unavailable(format!(
            "tab {tab_id} has no pane"
        ))),
        [pane] => Ok(pane.pane_id.clone()),
        _ => {
            let ids = panes
                .iter()
                .map(|pane| pane.pane_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(HerdrRunError::unavailable(format!(
                "tab {tab_id} has {} panes ({ids}); herdr-run needs an unsplit tab. Close the extra panes or point tab_name at a different tab.",
                panes.len()
            )))
        }
    }
}

fn cache_still_valid<A: HerdrApi + ?Sized>(
    client: &A,
    cached: &CacheRecord,
    workspace_label: &str,
    tab_label: &str,
) -> Result<bool> {
    if cached
        .workspace_label
        .as_deref()
        .is_some_and(|label| label != workspace_label)
        || cached
            .tab_label
            .as_deref()
            .is_some_and(|label| label != tab_label)
        || !client.pane_exists(&cached.pane_id)
    {
        return Ok(false);
    }
    // Resolve by the configured label, not merely by cached ID. Besides detecting ID reuse, this
    // exercises the client's duplicate-label refusal instead of letting a cache hide ambiguity. A
    // lookup that fails is a cache MISS, never a failed run: the cache is an optimisation, and the
    // label-based path below asks the same questions again and reports its own errors.
    match client.workspace_id_for_label(workspace_label) {
        Ok(Some(id)) if id == cached.workspace_id => {}
        _ => return Ok(false),
    }
    match client.tab_id_for_label(&cached.workspace_id, tab_label) {
        Ok(Some(id)) if id == cached.tab_id => {}
        _ => return Ok(false),
    }
    Ok(client
        .panes(Some(&cached.workspace_id))?
        .iter()
        .any(|pane| {
            pane.pane_id == cached.pane_id
                && pane.tab_id == cached.tab_id
                && pane.workspace_id == cached.workspace_id
        }))
}

fn load_cache(path: &Path, key: &str) -> Option<CacheRecord> {
    let contents = fs::read_to_string(path).ok()?;
    let document = strict_json(&contents)?;
    let value = document.as_object()?.get(key)?.clone();
    serde_json::from_value(value).ok()
}

fn store_cache(path: &Path, key: &str, target: &Target) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cache has no parent"))?;
    let mut parent_builder = fs::DirBuilder::new();
    parent_builder.recursive(true).mode(0o700).create(parent)?;
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(lock_path)?;
    lock.set_permissions(fs::Permissions::from_mode(0o600))?;
    FileExt::lock_exclusive(&lock)?;

    let mut entries = fs::read_to_string(path)
        .ok()
        .and_then(|contents| match strict_json(&contents) {
            Some(serde_json::Value::Object(entries)) => Some(entries),
            _ => None,
        })
        .unwrap_or_default();
    let record = CacheRecord {
        workspace_id: target.workspace_id.clone(),
        tab_id: target.tab_id.clone(),
        pane_id: target.pane_id.clone(),
        workspace_label: Some(target.workspace_label.clone()),
        tab_label: Some(target.tab_label.clone()),
    };
    entries.insert(
        key.to_owned(),
        serde_json::to_value(record).map_err(io::Error::other)?,
    );

    let temporary = unique_temporary(parent);
    let write_result =
        write_cache_temporary(&temporary, &entries).and_then(|()| fs::rename(&temporary, path));
    if write_result.is_ok() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn unique_temporary(parent: &Path) -> PathBuf {
    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".session-cache.{}.{}.tmp",
        std::process::id(),
        sequence
    ))
}

fn write_cache_temporary(
    path: &Path,
    entries: &serde_json::Map<String, serde_json::Value>,
) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let mut encoded = json_pretty(&serde_json::Value::Object(entries.clone())).into_bytes();
    encoded.push(b'\n');
    file.write_all(&encoded)?;
    file.flush()?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Arc, Barrier, Condvar, Mutex};
    use std::thread;

    use crate::client::{Pane, ProcessInfo};

    use super::*;

    /// The exact bytes and files a cache store leaves, shared with the other edition.
    #[test]
    fn golden_session_cache_store_bytes() {
        let document: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/record_cases.json"))
                .expect("golden record cases");
        for row in document["stored"].as_array().expect("stored rows") {
            let root = temporary_root("golden-store");
            let path = root.join("spool").join("session-cache.json");
            if let Some(existing) = row[0].as_str() {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, existing).unwrap();
            }
            let field = |index: usize| row[2][index].as_str().unwrap().to_owned();
            let target = Target {
                workspace_id: field(0),
                tab_id: field(1),
                pane_id: field(2),
                workspace_label: field(3),
                tab_label: field(4),
                created: Vec::new(),
                from_cache: false,
            };
            store_cache(&path, row[1].as_str().unwrap(), &target).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), row[3], "{row:?}");
            let mut names = fs::read_dir(path.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            names.sort();
            assert_eq!(serde_json::json!(names), row[4], "{row:?}");
            fs::remove_dir_all(root).unwrap();
        }
    }

    /// Session-cache entries shared with the other edition: which load, and as what.
    #[test]
    fn golden_session_cache_entries() {
        let document: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/record_cases.json"))
                .expect("golden record cases");
        let root = temporary_root("golden-cache");
        let path = root.join("session-cache.json");
        for row in document["cache"].as_array().expect("cache rows") {
            fs::write(&path, serde_json::to_string(&row[0]).unwrap()).unwrap();
            let shown =
                load_cache(&path, "agent-cmds\u{0}a").map_or(serde_json::Value::Null, |record| {
                    serde_json::json!([
                        record.workspace_id,
                        record.tab_id,
                        record.pane_id,
                        record.workspace_label,
                        record.tab_label
                    ])
                });
            assert_eq!(shown, row[1], "{row:?}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[derive(Clone)]
    struct FakeHerdr {
        state: Arc<Mutex<FakeState>>,
        create_gate: Option<Arc<CreateGate>>,
    }

    struct CreateGate {
        calls: AtomicUsize,
        entered: (Mutex<bool>, Condvar),
        released: (Mutex<bool>, Condvar),
    }

    impl CreateGate {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                entered: (Mutex::new(false), Condvar::new()),
                released: (Mutex::new(false), Condvar::new()),
            }
        }

        fn block_first_create(&self) {
            let ordinal = self.calls.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            if ordinal != 1 {
                return;
            }
            let (entered, entered_cv) = &self.entered;
            *entered.lock().expect("entered") = true;
            entered_cv.notify_all();
            let (released, released_cv) = &self.released;
            let mut release = released.lock().expect("released");
            while !*release {
                release = released_cv.wait(release).expect("release wait");
            }
        }

        fn wait_until_entered(&self) {
            let (entered, entered_cv) = &self.entered;
            let mut value = entered.lock().expect("entered");
            while !*value {
                value = entered_cv.wait(value).expect("entered wait");
            }
        }

        fn release(&self) {
            let (released, released_cv) = &self.released;
            *released.lock().expect("released") = true;
            released_cv.notify_all();
        }
    }

    struct FakeState {
        start_server: bool,
        workspace_id: Option<String>,
        workspace_label: Option<String>,
        tab_id: Option<String>,
        tab_label: Option<String>,
        panes: Vec<Pane>,
        calls: Vec<String>,
    }

    impl Default for FakeHerdr {
        fn default() -> Self {
            Self {
                state: Arc::new(Mutex::new(FakeState {
                    start_server: false,
                    workspace_id: None,
                    workspace_label: None,
                    tab_id: None,
                    tab_label: None,
                    panes: Vec::new(),
                    calls: Vec::new(),
                })),
                create_gate: None,
            }
        }
    }

    impl FakeHerdr {
        fn calls(&self) -> Vec<String> {
            self.state.lock().expect("state").calls.clone()
        }
    }

    impl HerdrApi for FakeHerdr {
        fn ensure_server(&self) -> Result<bool> {
            let mut state = self.state.lock().expect("state");
            state.calls.push("ensure".to_owned());
            Ok(std::mem::take(&mut state.start_server))
        }

        fn server_running(&self) -> bool {
            true
        }

        fn workspace_id_for_label(&self, label: &str) -> Result<Option<String>> {
            let mut state = self.state.lock().expect("state");
            state.calls.push(format!("workspace:{label}"));
            Ok((state.workspace_label.as_deref() == Some(label))
                .then(|| state.workspace_id.clone())
                .flatten())
        }

        fn create_workspace(&self, label: &str, _cwd: &str) -> Result<(String, String, String)> {
            if let Some(gate) = &self.create_gate {
                gate.block_first_create();
            }
            let mut state = self.state.lock().expect("state");
            state.calls.push("create-workspace".to_owned());
            state.workspace_id = Some("w1".to_owned());
            state.workspace_label = Some(label.to_owned());
            state.tab_id = Some("t1".to_owned());
            state.panes = vec![Pane {
                pane_id: "p1".to_owned(),
                tab_id: "t1".to_owned(),
                workspace_id: "w1".to_owned(),
            }];
            Ok(("w1".to_owned(), "t1".to_owned(), "p1".to_owned()))
        }

        fn tab_id_for_label(&self, workspace_id: &str, label: &str) -> Result<Option<String>> {
            let state = self.state.lock().expect("state");
            if state.workspace_id.as_deref() != Some(workspace_id)
                || state.tab_label.as_deref() != Some(label)
            {
                return Ok(None);
            }
            Ok(state.tab_id.clone())
        }

        fn create_tab(&self, workspace_id: &str, label: &str, _cwd: &str) -> Result<String> {
            let mut state = self.state.lock().expect("state");
            state.calls.push("create-tab".to_owned());
            state.tab_id = Some("t2".to_owned());
            state.tab_label = Some(label.to_owned());
            state.panes = vec![Pane {
                pane_id: "p2".to_owned(),
                tab_id: "t2".to_owned(),
                workspace_id: workspace_id.to_owned(),
            }];
            Ok("t2".to_owned())
        }

        fn rename_tab(&self, tab_id: &str, label: &str) -> Result<()> {
            let mut state = self.state.lock().expect("state");
            state.calls.push("rename-tab".to_owned());
            state.tab_id = Some(tab_id.to_owned());
            state.tab_label = Some(label.to_owned());
            Ok(())
        }

        fn panes(&self, workspace_id: Option<&str>) -> Result<Vec<Pane>> {
            let state = self.state.lock().expect("state");
            Ok(state
                .panes
                .iter()
                .filter(|pane| workspace_id.is_none_or(|wanted| pane.workspace_id == wanted))
                .cloned()
                .collect())
        }

        fn pane_exists(&self, pane_id: &str) -> bool {
            self.state
                .lock()
                .expect("state")
                .panes
                .iter()
                .any(|pane| pane.pane_id == pane_id)
        }

        fn process_info(&self, _pane_id: &str) -> Result<ProcessInfo> {
            unreachable!()
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
            "herdr-run-session-{name}-{}-{}",
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("temporary root");
        path
    }

    fn config(root: &Path) -> Config {
        Config {
            project_root: root.to_string_lossy().into_owned(),
            ..Config::default()
        }
    }

    /// A workspace holding a fixed pane population, so the cap can be checked in both directions.
    ///
    /// The shared `FakeHerdr` above models one tab at a time, which cannot express "the workspace
    /// is already full"; this one carries a pane list and records whether a tab was created.
    struct CapFake {
        panes: Mutex<Vec<Pane>>,
        created: AtomicUsize,
        idle: Mutex<BTreeMap<String, i64>>,
        closed: Mutex<Vec<String>>,
        pane_prefix: String,
        /// When set, `tab close` reports success but the listing never changes.
        sticky_close: bool,
    }

    impl CapFake {
        fn holding(count: usize) -> Self {
            Self::holding_panes("p", count)
        }

        /// Pane IDs carry `pane_prefix`, so a test that takes real pane locks can use IDs that no
        /// concurrently running test shares.
        fn holding_panes(pane_prefix: &str, count: usize) -> Self {
            Self {
                panes: Mutex::new(
                    (0..count)
                        .map(|index| Pane {
                            pane_id: format!("{pane_prefix}{index}"),
                            tab_id: format!("t{index}"),
                            workspace_id: "w1".to_owned(),
                        })
                        .collect(),
                ),
                created: AtomicUsize::new(0),
                idle: Mutex::new(BTreeMap::new()),
                closed: Mutex::new(Vec::new()),
                pane_prefix: pane_prefix.to_owned(),
                sticky_close: false,
            }
        }

        fn closed(&self) -> Vec<String> {
            self.closed.lock().expect("closed").clone()
        }

        fn creates(&self) -> usize {
            self.created.load(AtomicOrdering::SeqCst)
        }
    }

    impl HerdrApi for CapFake {
        fn ensure_server(&self) -> Result<bool> {
            Ok(false)
        }

        fn server_running(&self) -> bool {
            true
        }

        fn workspace_id_for_label(&self, _label: &str) -> Result<Option<String>> {
            Ok(Some("w1".to_owned()))
        }

        fn create_workspace(&self, _label: &str, _cwd: &str) -> Result<(String, String, String)> {
            unreachable!("the workspace already exists")
        }

        fn tab_id_for_label(&self, _workspace_id: &str, label: &str) -> Result<Option<String>> {
            let panes = self.panes.lock().expect("panes");
            Ok(panes
                .iter()
                .find(|pane| pane.tab_id == format!("t{}", label.trim_start_matches("filler-")))
                .filter(|_| label.starts_with("filler-"))
                .map(|pane| pane.tab_id.clone()))
        }

        fn create_tab(&self, workspace_id: &str, _label: &str, _cwd: &str) -> Result<String> {
            let mut panes = self.panes.lock().expect("panes");
            let index = panes.len() + self.closed.lock().expect("closed").len();
            self.created.fetch_add(1, AtomicOrdering::SeqCst);
            panes.push(Pane {
                pane_id: format!("{}{index}", self.pane_prefix),
                tab_id: format!("t{index}"),
                workspace_id: workspace_id.to_owned(),
            });
            Ok(format!("t{index}"))
        }

        fn rename_tab(&self, _tab_id: &str, _label: &str) -> Result<()> {
            Ok(())
        }

        fn panes(&self, _workspace_id: Option<&str>) -> Result<Vec<Pane>> {
            Ok(self.panes.lock().expect("panes").clone())
        }

        fn pane_exists(&self, pane_id: &str) -> bool {
            self.panes
                .lock()
                .expect("panes")
                .iter()
                .any(|pane| pane.pane_id == pane_id)
        }

        fn close_tab(&self, tab_id: &str) -> Result<()> {
            let mut closed = self.closed.lock().expect("closed");
            closed.push(tab_id.to_owned());
            if self.sticky_close {
                assert!(closed.len() <= 10, "runaway eviction: {closed:?}");
                return Ok(());
            }
            drop(closed);
            self.panes
                .lock()
                .expect("panes")
                .retain(|pane| pane.tab_id != tab_id);
            Ok(())
        }

        /// Every pane runs `git push` except those given an idle shell.
        fn process_info(&self, pane_id: &str) -> Result<ProcessInfo> {
            let idle = self.idle.lock().expect("idle").get(pane_id).copied();
            Ok(match idle {
                Some(pid) => ProcessInfo {
                    pane_id: pane_id.to_owned(),
                    shell_pid: pid,
                    foreground_pgid: pid,
                    foreground: vec![(pid, "bash".to_owned(), "/bin/bash".to_owned())],
                },
                None => ProcessInfo {
                    pane_id: pane_id.to_owned(),
                    shell_pid: 1,
                    foreground_pgid: 2,
                    foreground: vec![(2, "git".to_owned(), "git push".to_owned())],
                },
            })
        }

        fn read(&self, _pane_id: &str, _source: &str, _lines: Option<usize>) -> Result<String> {
            unreachable!()
        }

        fn run(&self, _pane_id: &str, _command: &str) -> Result<()> {
            unreachable!()
        }
    }

    // Two directions, because a cap that refuses everything also passes a refusal test.

    #[test]
    fn pane_cap_refuses_a_new_tab_once_the_workspace_is_full() {
        let root = temporary_root("cap-full");
        let mut config = config(&root);
        config.max_panes = 3;
        let fake = CapFake::holding(3);
        let error = resolve_target(&fake, &config, "one-too-many", false)
            .expect_err("the cap must refuse a fourth tab");
        assert!(error.to_string().contains("max_panes is 3"), "{error}");
        // Every tab is running a command, so none may be replaced; the refusal says why.
        assert!(
            error
                .to_string()
                .contains("None of its 3 tab(s) could be replaced"),
            "{error}"
        );
        // Refused BEFORE creating anything: the workspace must not grow past the cap even by one.
        assert_eq!(fake.creates(), 0);
        assert!(fake.closed().is_empty());
        assert_eq!(fake.panes(None).unwrap().len(), 3);
        fs::remove_dir_all(root).unwrap();
    }

    /// A real session-leading process, so the replacement path reads the live `/proc`.
    fn idle_session_leader() -> std::process::Child {
        std::process::Command::new("setsid")
            .args(["sleep", "60"])
            .spawn()
            .expect("spawn a session leader")
    }

    #[test]
    fn pane_cap_replaces_the_least_recently_used_idle_tab() {
        let root = temporary_root("cap-replace");
        let mut config = config(&root);
        config.max_panes = 3;
        let prefix = format!("cap-replace-{}-p", std::process::id());
        let fake = CapFake::holding_panes(&prefix, 3);
        let mut leader = idle_session_leader();
        let pid = i64::from(leader.id());
        // `setsid` execs `sleep` in place; wait until the child really leads its own session.
        for _ in 0..200 {
            if evict::scan_session(pid, Path::new("/proc")).shell_sid == Some(pid) {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        fake.idle
            .lock()
            .expect("idle")
            .insert(format!("{prefix}1"), pid);
        let target = resolve_target(&fake, &config, "newcomer", false).expect("replaced");
        let _ = leader.kill();
        let _ = leader.wait();
        assert_eq!(target.created, ["tab"]);
        assert_eq!(fake.closed(), ["t1"]);
        assert_eq!(fake.creates(), 1);
        assert_eq!(fake.panes(None).unwrap().len(), 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pane_cap_stops_closing_when_the_listing_never_shrinks() {
        let root = temporary_root("cap-sticky");
        let mut config = config(&root);
        config.max_panes = 3;
        let prefix = format!("cap-sticky-{}-p", std::process::id());
        let mut fake = CapFake::holding_panes(&prefix, 4);
        fake.sticky_close = true;
        let mut leader = idle_session_leader();
        let pid = i64::from(leader.id());
        for _ in 0..200 {
            if evict::scan_session(pid, Path::new("/proc")).shell_sid == Some(pid) {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        fake.idle
            .lock()
            .expect("idle")
            .insert(format!("{prefix}0"), pid);
        let error = enforce_pane_cap(&fake, &config, "w1", "newcomer", Path::new("/proc"))
            .expect_err("a listing that never shrinks must end in a refusal");
        let _ = leader.kill();
        let _ = leader.wait();
        // One close per pane over the cap, plus the one that makes room: never an endless loop.
        assert_eq!(fake.closed(), ["t0", "t0"]);
        assert!(error.to_string().contains("max_panes is 3"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pane_cap_leaves_room_up_to_the_limit() {
        let root = temporary_root("cap-room");
        let mut config = config(&root);
        config.max_panes = 3;
        let fake = CapFake::holding(2);
        let target = resolve_target(&fake, &config, "third-agent", false).expect("third tab");
        assert_eq!(target.created, ["tab"]);
        assert_eq!(fake.panes(None).unwrap().len(), 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pane_cap_never_locks_an_agent_out_of_its_existing_tab() {
        let root = temporary_root("cap-existing");
        let mut config = config(&root);
        config.max_panes = 3;
        let fake = CapFake::holding(3);
        let target = resolve_target(&fake, &config, "filler-1", false).expect("existing tab");
        assert_eq!(target.created, Vec::<String>::new());
        assert_eq!(fake.creates(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pane_cap_of_zero_is_unlimited() {
        let root = temporary_root("cap-zero");
        let mut config = config(&root);
        config.max_panes = 0;
        let fake = CapFake::holding(9);
        let target = resolve_target(&fake, &config, "tenth-agent", false).expect("tenth tab");
        assert_eq!(target.created, ["tab"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pane_cap_names_the_remedy_rather_than_only_refusing() {
        let root = temporary_root("cap-remedy");
        let mut config = config(&root);
        config.max_panes = 1;
        let fake = CapFake::holding(1);
        let error = resolve_target(&fake, &config, "another", false).expect_err("cap refusal");
        let message = error.to_string();
        assert!(message.contains("herdr-run reap"), "{message}");
        assert!(message.contains("max_panes"), "{message}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restricted_tab_renderer_accepts_only_exact_fields_and_escaped_braces() {
        let root = temporary_root("render").join("project-x");
        fs::create_dir_all(&root).unwrap();
        let mut config = config(&root);
        config.tab_name = "{{{project}}}:{agent}".to_owned();
        assert_eq!(
            tab_label_for(&config, "worker").unwrap(),
            "{project-x}:worker"
        );
        for invalid in [
            "{}",
            "{agent.name}",
            "{agent!r}",
            "{agent:>3}",
            "{agent:",
            "x}",
        ] {
            config.tab_name = invalid.to_owned();
            assert!(tab_label_for(&config, "worker").is_err(), "{invalid}");
        }
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn first_resolution_reuses_root_tab_and_second_uses_validated_cache() {
        let root = temporary_root("cache-hit");
        let config = config(&root);
        let fake = FakeHerdr::default();
        let first = resolve_target(&fake, &config, "agent", true).unwrap();
        assert_eq!(first.created, ["workspace", "tab"]);
        assert!(!first.from_cache);
        let second = resolve_target(&fake, &config, "agent", true).unwrap();
        assert!(second.from_cache);
        assert_eq!(first.pane_id, second.pane_id);
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|call| call.as_str() == "create-workspace")
                .count(),
            1
        );
        let path = cache_path(&config);
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(format!("{}.lock", path.display()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_first_resolution_across_projects_creates_session_once() {
        let root = temporary_root("concurrent-resolution");
        let project_one = root.join("project-one");
        let project_two = root.join("project-two");
        fs::create_dir_all(&project_one).unwrap();
        fs::create_dir_all(&project_two).unwrap();
        let first_config = config(&project_one);
        let second_config = config(&project_two);
        let gate = Arc::new(CreateGate::new());
        let fake = FakeHerdr {
            create_gate: Some(gate.clone()),
            ..FakeHerdr::default()
        };

        let first_fake = fake.clone();
        let first =
            thread::spawn(move || resolve_target(&first_fake, &first_config, "agent", true));
        gate.wait_until_entered();

        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let second_fake = fake.clone();
        let second = thread::spawn(move || {
            started_tx.send(()).unwrap();
            resolve_target(&second_fake, &second_config, "agent", true)
        });
        started_rx.recv().unwrap();
        thread::sleep(std::time::Duration::from_millis(100));
        let calls_while_first_blocked = gate.calls.load(AtomicOrdering::SeqCst);
        gate.release();

        let first_target = first.join().unwrap().unwrap();
        let second_target = second.join().unwrap().unwrap();
        assert_eq!(calls_while_first_blocked, 1);
        assert_eq!(gate.calls.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(first_target.pane_id, second_target.pane_id);
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|call| call.as_str() == "create-workspace")
                .count(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_rejects_reused_workspace_id_with_wrong_label() {
        let root = temporary_root("stale-label");
        let config = config(&root);
        let fake = FakeHerdr::default();
        resolve_target(&fake, &config, "agent", true).unwrap();
        {
            let mut state = fake.state.lock().unwrap();
            state.workspace_label = Some("someone-else".to_owned());
        }
        let second = resolve_target(&fake, &config, "agent", true).unwrap();
        assert!(!second.from_cache);
        assert_eq!(second.created, ["workspace", "tab"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn split_tab_is_refused_instead_of_selecting_an_arbitrary_pane() {
        let root = temporary_root("split");
        let config = config(&root);
        let fake = FakeHerdr::default();
        resolve_target(&fake, &config, "agent", true).unwrap();
        {
            let mut state = fake.state.lock().unwrap();
            state.panes.push(Pane {
                pane_id: "p-extra".to_owned(),
                tab_id: "t1".to_owned(),
                workspace_id: "w1".to_owned(),
            });
        }
        let error = resolve_target(&fake, &config, "agent", false).unwrap_err();
        assert!(error.to_string().contains("needs an unsplit tab"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_cache_writers_preserve_both_entries() {
        let root = temporary_root("writers");
        let config = config(&root);
        let path = cache_path(&config);
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for index in 0..2 {
            let barrier = barrier.clone();
            let path = path.clone();
            workers.push(thread::spawn(move || {
                let target = Target {
                    workspace_id: format!("w{index}"),
                    tab_id: format!("t{index}"),
                    pane_id: format!("p{index}"),
                    workspace_label: "workspace".to_owned(),
                    tab_label: format!("agent-{index}"),
                    created: Vec::new(),
                    from_cache: false,
                };
                barrier.wait();
                store_cache(&path, &format!("key-{index}"), &target).unwrap();
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        let document: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(document.len(), 2);
        assert!(document.contains_key("key-0"));
        assert!(document.contains_key("key-1"));
        fs::remove_dir_all(root).unwrap();
    }
}
