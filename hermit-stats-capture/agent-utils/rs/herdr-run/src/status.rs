//! The non-mutating report behind `herdr-run status`.
//!
//! `status` is to this command what `git status` is to git: it answers "what would happen if I ran
//! something here?" and it answers it by looking. It resolves the configuration in effect, states
//! the policy that configuration produces, and asks the live session what it already contains —
//! and it creates nothing along the way. A status command that brought up a server, a workspace,
//! or a tab in order to describe them would be reporting on its own side effects.

use serde_json::{json, Value};

use crate::client::HerdrApi;
use crate::config::{describe_missing, missing_defaults, Config, ALLOW_ANY_PROGRAM};
use crate::textfmt::{display, quote};

/// What could be learned about this project's live Herdr session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Session {
    /// The Herdr command could not be resolved from a trusted install location.
    Unreachable(String),
    /// Herdr is installed, but no server is running.
    NotRunning,
    /// A server is running and answered; `None` means no workspace carries the configured label.
    Running(Option<WorkspaceView>),
    /// A server is running but a control call failed, so the session cannot be described.
    Failed(String),
}

/// What the running server says about the configured workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceView {
    /// Panes currently in the workspace, which is what `max_panes` is measured against.
    pub panes: usize,
    /// Whether this agent's tab already exists.
    pub tab_exists: bool,
}

/// Ask the live session what it contains. Creates nothing.
pub fn inspect_session<A: HerdrApi + ?Sized>(
    client: &A,
    config: &Config,
    tab_label: &str,
) -> Session {
    if !client.server_running() {
        return Session::NotRunning;
    }
    let workspace_id = match client.workspace_id_for_label(&config.workspace) {
        Ok(Some(workspace_id)) => workspace_id,
        Ok(None) => return Session::Running(None),
        Err(error) => return Session::Failed(error.message().to_owned()),
    };
    let panes = match client.panes(Some(&workspace_id)) {
        Ok(panes) => panes.len(),
        Err(error) => return Session::Failed(error.message().to_owned()),
    };
    let tab_exists = match client.tab_id_for_label(&workspace_id, tab_label) {
        Ok(tab) => tab.is_some(),
        Err(error) => return Session::Failed(error.message().to_owned()),
    };
    Session::Running(Some(WorkspaceView { panes, tab_exists }))
}

fn herdr_detail(session: &Session) -> String {
    match session {
        Session::Unreachable(reason) => format!("not reachable — {reason}"),
        Session::NotRunning => "installed; no server is running".to_owned(),
        Session::Running(_) | Session::Failed(_) => "installed; server is running".to_owned(),
    }
}

fn panes_detail(session: &Session, workspace: &str) -> String {
    match session {
        Session::Unreachable(_) => "unknown (herdr is not reachable)".to_owned(),
        Session::NotRunning => "unknown (no server is running)".to_owned(),
        Session::Failed(reason) => format!("unknown ({reason})"),
        Session::Running(None) => format!("no workspace labelled {} exists yet", quote(workspace)),
        Session::Running(Some(view)) => {
            format!("{} in workspace {}", view.panes, quote(workspace))
        }
    }
}

fn tab_detail(session: &Session, tab_label: &str) -> String {
    let shown = display(tab_label);
    match session {
        Session::Running(Some(view)) if view.tab_exists => format!("{shown} (exists)"),
        Session::Running(_) => format!("{shown} (not created yet)"),
        _ => shown,
    }
}

fn display_join(values: &[String]) -> String {
    values
        .iter()
        .map(|value| display(value))
        .collect::<Vec<_>>()
        .join(", ")
}

fn allow_detail(config: &Config) -> String {
    if config.allows_any_program() {
        return format!("any program ({ALLOW_ANY_PROGRAM:?})");
    }
    display_join(&config.allow)
}

/// Whether every built-in deny entry is in force, or which ones the file leaves out.
fn deny_detail(config: &Config) -> String {
    let missing = missing_defaults(config);
    if missing.is_empty() {
        return "every built-in entry is in force".to_owned();
    }
    format!("leaves out built-in {}", describe_missing(&missing))
}

fn list_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "(none)".to_owned()
    } else {
        display_join(values)
    }
}

/// Render the human-readable report, in the shape `git status` set the expectation for.
///
/// Every value goes through [`display`], so a path or a name holding a terminal control or an
/// invisible character is shown quoted rather than raw.
#[must_use]
pub fn status_text(config: &Config, agent: &str, tab_label: &str, session: &Session) -> String {
    let mut text = String::from("herdr-run status\n\n");
    text.push_str("  configuration\n");
    text.push_str(&format!(
        "    file          {}\n",
        config
            .source_path
            .as_deref()
            .map_or_else(|| "(built-in defaults)".to_owned(), display)
    ));
    text.push_str(&format!(
        "    project root  {}\n",
        display(&config.project_root)
    ));
    text.push_str(&format!(
        "    spool dir     {}\n",
        display(&config.spool_dir)
    ));
    text.push_str("\n  policy\n");
    text.push_str(&format!("    allow         {}\n", allow_detail(config)));
    text.push_str(&format!(
        "    prefixes      {}\n",
        list_or_none(&config.prefixes)
    ));
    text.push_str(&format!("    deny lists    {}\n", deny_detail(config)));
    text.push_str("\n  session\n");
    text.push_str(&format!("    agent         {}\n", display(agent)));
    text.push_str(&format!(
        "    workspace     {}\n",
        display(&config.workspace)
    ));
    text.push_str(&format!(
        "    tab label     {}\n",
        tab_detail(session, tab_label)
    ));
    text.push_str(&format!("    herdr         {}\n", herdr_detail(session)));
    text.push_str(&format!(
        "    panes         {}\n",
        panes_detail(session, &config.workspace)
    ));
    text.push_str("\nNothing was changed: status only reads.\n");
    text
}

/// Render the machine-readable report.
#[must_use]
pub fn status_document(config: &Config, agent: &str, tab_label: &str, session: &Session) -> Value {
    let view = match session {
        Session::Running(view) => *view,
        _ => None,
    };
    let workspace_exists = match session {
        Session::Running(inner) => Some(inner.is_some()),
        _ => None,
    };
    json!({
        "agent": agent,
        "allow": config.allow,
        "allow_any_program": config.allows_any_program(),
        "config_file": config.source_path,
        "missing_defaults": missing_defaults(config)
            .iter()
            .map(|item| json!({"entries": item.entries, "setting": item.setting}))
            .collect::<Vec<_>>(),
        "herdr": {
            "detail": herdr_detail(session),
            "reachable": !matches!(session, Session::Unreachable(_)),
            "server_running": matches!(session, Session::Running(_) | Session::Failed(_)),
        },
        "panes": {
            "count": view.map(|view| view.panes),
            "detail": panes_detail(session, &config.workspace),
            "workspace_exists": workspace_exists,
        },
        "prefixes": config.prefixes,
        "project_root": config.project_root,
        "spool_dir": config.spool_dir,
        "tab": {
            "exists": view.map(|view| view.tab_exists),
            "label": tab_label,
        },
        "workspace": config.workspace,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Pane, ProcessInfo};
    use crate::error::{HerdrRunError, Result};
    use std::cell::RefCell;

    /// A session that answers reads and PANICS on anything that would change the session.
    ///
    /// Naming every mutating call is the point. `status` is documented as strictly non-mutating,
    /// and the failure this guards against is not a wrong number in the report but a status
    /// command that brings up a server, a workspace, or a tab in order to describe it — which
    /// would be indistinguishable from a healthy session the next time anybody looked.
    struct ReadOnlyFake {
        running: bool,
        workspace: Option<&'static str>,
        panes: usize,
        tab: bool,
        calls: RefCell<Vec<&'static str>>,
    }

    impl ReadOnlyFake {
        fn running_with(workspace: Option<&'static str>, panes: usize, tab: bool) -> Self {
            Self {
                running: true,
                workspace,
                panes,
                tab,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl HerdrApi for ReadOnlyFake {
        fn ensure_server(&self) -> Result<bool> {
            panic!("status must never start a server")
        }
        fn server_running(&self) -> bool {
            self.calls.borrow_mut().push("server_running");
            self.running
        }
        fn workspace_id_for_label(&self, _label: &str) -> Result<Option<String>> {
            self.calls.borrow_mut().push("workspace_id_for_label");
            Ok(self.workspace.map(str::to_owned))
        }
        fn create_workspace(&self, _label: &str, _cwd: &str) -> Result<(String, String, String)> {
            panic!("status must never create a workspace")
        }
        fn tab_id_for_label(&self, _workspace_id: &str, _label: &str) -> Result<Option<String>> {
            self.calls.borrow_mut().push("tab_id_for_label");
            Ok(self.tab.then(|| "w1:t1".to_owned()))
        }
        fn create_tab(&self, _workspace_id: &str, _label: &str, _cwd: &str) -> Result<String> {
            panic!("status must never create a tab")
        }
        fn rename_tab(&self, _tab_id: &str, _label: &str) -> Result<()> {
            panic!("status must never rename a tab")
        }
        fn panes(&self, _workspace_id: Option<&str>) -> Result<Vec<Pane>> {
            self.calls.borrow_mut().push("panes");
            Ok((0..self.panes)
                .map(|index| Pane {
                    pane_id: format!("w1:p{index}"),
                    tab_id: "w1:t1".to_owned(),
                    workspace_id: "w1".to_owned(),
                })
                .collect())
        }
        fn pane_exists(&self, _pane_id: &str) -> bool {
            true
        }
        fn process_info(&self, _pane_id: &str) -> Result<ProcessInfo> {
            panic!("status must never read foreground process state")
        }
        fn read(&self, _pane_id: &str, _source: &str, _lines: Option<usize>) -> Result<String> {
            panic!("status must never read pane contents")
        }
        fn run(&self, _pane_id: &str, _command: &str) -> Result<()> {
            panic!("status must never type into a pane")
        }
    }

    #[test]
    fn a_stopped_server_is_reported_without_being_started() {
        let fake = ReadOnlyFake {
            running: false,
            workspace: None,
            panes: 0,
            tab: false,
            calls: RefCell::new(Vec::new()),
        };
        let session = inspect_session(&fake, &Config::default(), "kvm");
        assert_eq!(session, Session::NotRunning);
        assert_eq!(*fake.calls.borrow(), ["server_running"]);
    }

    #[test]
    fn a_running_server_reports_panes_and_whether_the_tab_exists() {
        let fake = ReadOnlyFake::running_with(Some("w1"), 3, true);
        assert_eq!(
            inspect_session(&fake, &Config::default(), "kvm"),
            Session::Running(Some(WorkspaceView {
                panes: 3,
                tab_exists: true,
            }))
        );
        assert_eq!(
            *fake.calls.borrow(),
            [
                "server_running",
                "workspace_id_for_label",
                "panes",
                "tab_id_for_label"
            ]
        );

        let fake = ReadOnlyFake::running_with(None, 0, false);
        assert_eq!(
            inspect_session(&fake, &Config::default(), "kvm"),
            Session::Running(None)
        );
    }

    #[test]
    fn a_failing_control_call_is_reported_rather_than_guessed_at() {
        struct FailingWorkspace(ReadOnlyFake);
        impl HerdrApi for FailingWorkspace {
            fn ensure_server(&self) -> Result<bool> {
                self.0.ensure_server()
            }
            fn server_running(&self) -> bool {
                self.0.server_running()
            }
            fn workspace_id_for_label(&self, _label: &str) -> Result<Option<String>> {
                Err(HerdrRunError::unavailable("herdr workspace list timed out"))
            }
            fn create_workspace(&self, label: &str, cwd: &str) -> Result<(String, String, String)> {
                self.0.create_workspace(label, cwd)
            }
            fn tab_id_for_label(&self, workspace: &str, label: &str) -> Result<Option<String>> {
                self.0.tab_id_for_label(workspace, label)
            }
            fn create_tab(&self, workspace: &str, label: &str, cwd: &str) -> Result<String> {
                self.0.create_tab(workspace, label, cwd)
            }
            fn rename_tab(&self, tab: &str, label: &str) -> Result<()> {
                self.0.rename_tab(tab, label)
            }
            fn panes(&self, workspace: Option<&str>) -> Result<Vec<Pane>> {
                self.0.panes(workspace)
            }
            fn pane_exists(&self, pane: &str) -> bool {
                self.0.pane_exists(pane)
            }
            fn process_info(&self, pane: &str) -> Result<ProcessInfo> {
                self.0.process_info(pane)
            }
            fn read(&self, pane: &str, source: &str, lines: Option<usize>) -> Result<String> {
                self.0.read(pane, source, lines)
            }
            fn run(&self, pane: &str, command: &str) -> Result<()> {
                self.0.run(pane, command)
            }
        }
        let fake = FailingWorkspace(ReadOnlyFake::running_with(Some("w1"), 0, false));
        assert_eq!(
            inspect_session(&fake, &Config::default(), "kvm"),
            Session::Failed("herdr workspace list timed out".to_owned())
        );
    }

    #[test]
    fn the_report_names_the_configuration_the_policy_and_the_session() {
        let config = Config {
            source_path: Some("/project/.herdr-run.yaml".to_owned()),
            project_root: "/project".to_owned(),
            ..Config::default()
        };
        let session = Session::Running(Some(WorkspaceView {
            panes: 3,
            tab_exists: false,
        }));
        let text = status_text(&config, "kvm", "kvm", &session);
        for line in [
            "    file          /project/.herdr-run.yaml\n",
            "    project root  /project\n",
            "    spool dir     .herdr-run\n",
            "    allow         git, gh\n",
            "    prefixes      with-proxy\n",
            "    deny lists    every built-in entry is in force\n",
            "    agent         kvm\n",
            "    workspace     agent-cmds\n",
            "    tab label     kvm (not created yet)\n",
            "    herdr         installed; server is running\n",
            "    panes         3 in workspace \"agent-cmds\"\n",
            "\nNothing was changed: status only reads.\n",
        ] {
            assert!(text.contains(line), "status omitted {line:?}:\n{text}");
        }

        let document = status_document(&config, "kvm", "kvm", &session);
        assert_eq!(document["panes"]["count"], json!(3));
        assert_eq!(document["panes"]["workspace_exists"], json!(true));
        assert_eq!(document["tab"]["exists"], json!(false));
        assert_eq!(document["herdr"]["reachable"], json!(true));
        assert_eq!(document["herdr"]["server_running"], json!(true));
        assert_eq!(document["allow_any_program"], json!(false));
        assert_eq!(document["missing_defaults"], json!([]));
    }

    /// A file written before an entry joined the built-in defaults does not enforce it; say so.
    #[test]
    fn a_deny_list_that_leaves_out_built_in_entries_is_reported() {
        let mut config = Config {
            source_path: Some("/project/.herdr-run.yaml".to_owned()),
            project_root: "/project".to_owned(),
            deny_anywhere: vec!["--upload-pack".to_owned(), "--receive-pack".to_owned()],
            ..Config::default()
        };
        config
            .deny_global
            .insert("git".to_owned(), vec!["-c".to_owned()]);
        let text = status_text(&config, "kvm", "kvm", &Session::NotRunning);
        assert!(
            text.contains(
                "    deny lists    leaves out built-in deny_global.git \"--config-env\", \"--exec-path\", \"--namespace\"; deny_anywhere \"--exec\"\n"
            ),
            "{text}"
        );
        assert_eq!(
            status_document(&config, "kvm", "kvm", &Session::NotRunning)["missing_defaults"],
            json!([
                {"entries": ["--config-env", "--exec-path", "--namespace"], "setting": "deny_global.git"},
                {"entries": ["--exec"], "setting": "deny_anywhere"},
            ])
        );
    }

    #[test]
    fn an_unreachable_herdr_is_said_plainly_and_leaves_the_counts_unknown() {
        let session = Session::Unreachable("no Herdr executable found".to_owned());
        let text = status_text(&Config::default(), "kvm", "kvm", &session);
        assert!(
            text.contains("    herdr         not reachable — no Herdr executable found\n"),
            "{text}"
        );
        assert!(
            text.contains("    panes         unknown (herdr is not reachable)\n"),
            "{text}"
        );
        let document = status_document(&Config::default(), "kvm", "kvm", &session);
        assert_eq!(document["herdr"]["reachable"], json!(false));
        assert_eq!(document["panes"]["count"], Value::Null);
        assert_eq!(document["panes"]["workspace_exists"], Value::Null);
        assert_eq!(document["tab"]["exists"], Value::Null);
    }

    #[test]
    fn the_allow_everything_mode_is_reported_as_such_rather_than_as_a_program_named_star() {
        let config = Config {
            allow: vec!["*".to_owned()],
            prefixes: Vec::new(),
            ..Config::default()
        };
        let text = status_text(&config, "kvm", "kvm", &Session::NotRunning);
        assert!(
            text.contains("    allow         any program (\"*\")\n"),
            "{text}"
        );
        assert!(text.contains("    prefixes      (none)\n"), "{text}");
        assert_eq!(
            status_document(&config, "kvm", "kvm", &Session::NotRunning)["allow_any_program"],
            json!(true)
        );
    }
}
