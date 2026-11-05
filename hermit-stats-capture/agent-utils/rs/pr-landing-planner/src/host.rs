//! VCS-host seam and the production GitHub/`git` implementation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Map, Value};

use crate::classify::parse_rollup;
use crate::context::{retirement_record, ALLOWED_RETIREMENT_PERMISSIONS};
use crate::model::{RawPr, ReviewEvidenceEvent, ReviewEvidenceSnapshot, NATIVE_REVIEW_STATES};

#[derive(Clone, Debug)]
struct HostEnrichment {
    checks: Vec<crate::model::CheckRun>,
    review_snapshot: ReviewEvidenceSnapshot,
}

/// Read-only repository-host and local Git operations required by collection.
pub trait VcsHost {
    /// List open pull requests, optionally scoped by a target base.
    fn list_open_prs(&mut self, repo: &str, base: Option<&str>) -> Result<Vec<RawPr>, String>;
    /// Fetch sources into private destinations and return resolved SHAs by destination.
    fn prefetch_refs(
        &mut self,
        refspecs: &[(String, String)],
    ) -> Result<BTreeMap<String, String>, String>;
    /// Return paths that conflict when merging two commit identities.
    fn merge_tree(&mut self, left: &str, right: &str) -> Result<Vec<String>, String>;
    /// Return whether one commit is an ancestor of another.
    fn is_ancestor(&mut self, ancestor: &str, descendant: &str) -> Result<bool, String>;
    /// Return files changed from the merge base through the head.
    fn changed_files(&mut self, base_sha: &str, head_sha: &str)
        -> Result<BTreeSet<String>, String>;
    /// Return how many commits from the base are absent from the head.
    fn commits_behind(&mut self, head_sha: &str, base_sha: &str) -> Result<i64, String>;
}

const LIGHT_FIELDS: [&str; 14] = [
    "number",
    "title",
    "author",
    "baseRefName",
    "headRefName",
    "headRefOid",
    "isDraft",
    "mergeable",
    "reviewDecision",
    "createdAt",
    "updatedAt",
    "additions",
    "deletions",
    "labels",
];
const ENRICHMENT_WORKERS: usize = 8;
const PR_LIST_LIMIT: usize = 500;
const REVIEWS_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!, $endCursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      headRefOid
      reviews(first: 100, after: $endCursor) {
        nodes { id author { login } state commit { oid } submittedAt updatedAt lastEditedAt body }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"#;
const ISSUE_COMMENTS_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!, $endCursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      headRefOid
      comments(first: 100, after: $endCursor) {
        nodes { id author { login } body createdAt updatedAt isMinimized minimizedReason }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"#;
const INLINE_STATE_FIELDS: [&str; 13] = [
    "in_reply_to_id",
    "line",
    "original_commit_id",
    "original_line",
    "original_position",
    "original_start_line",
    "path",
    "position",
    "pull_request_review_id",
    "side",
    "start_line",
    "start_side",
    "subject_type",
];

/// Production adapter using `gh` for metadata and a local Git clone for graph probes.
pub struct GitHubHost {
    git_dir: PathBuf,
    remote: String,
    wrapper: Vec<String>,
    gh: String,
}

impl GitHubHost {
    /// Construct an adapter for a clone, remote, optional command prefix, and `gh` executable.
    pub fn new(git_dir: PathBuf, remote: String, wrapper: Vec<String>, gh: String) -> Self {
        Self {
            git_dir,
            remote,
            wrapper,
            gh,
        }
    }

    fn run(&self, args: &[String], cwd: Option<&Path>, allowed: &[i32]) -> Result<Output, String> {
        let (program, rest) = args
            .split_first()
            .ok_or_else(|| "cannot run an empty command".to_owned())?;
        let mut command = Command::new(program);
        command.args(rest);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command.output().map_err(|error| {
            format!(
                "failed to run {}: {error}",
                shell_words::join(args.to_vec())
            )
        })?;
        let code = output.status.code().unwrap_or(-1);
        if !allowed.contains(&code) {
            return Err(format!(
                "command failed ({code}): {}\n{}",
                shell_words::join(args.to_vec()),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(output)
    }

    fn net(&self, command: Vec<String>) -> Vec<String> {
        self.wrapper.iter().cloned().chain(command).collect()
    }

    fn graphql_connection(
        &self,
        repo: &str,
        number: i64,
        connection: &str,
        query: &str,
    ) -> Result<(String, Vec<Value>), String> {
        let mut repo_parts = repo.rsplit('/');
        let name = repo_parts.next().filter(|value| !value.is_empty());
        let owner = repo_parts.next().filter(|value| !value.is_empty());
        let (Some(owner), Some(name)) = (owner, name) else {
            return Err(format!("repository {repo:?} lacks owner/name"));
        };
        let args = self.net(vec![
            self.gh.clone(),
            "api".into(),
            "graphql".into(),
            "--paginate".into(),
            "--slurp".into(),
            "-f".into(),
            format!("query={query}"),
            "-F".into(),
            format!("owner={owner}"),
            "-F".into(),
            format!("name={name}"),
            "-F".into(),
            format!("number={number}"),
        ]);
        let output = self.run(&args, None, &[0])?;
        graphql_connection_from_slurp(&output.stdout, number, connection)
    }

    fn retirement_permission(&self, repo: &str, author: &str) -> Result<String, String> {
        let args = self.net(vec![
            self.gh.clone(),
            "api".into(),
            format!("repos/{repo}/collaborators/{author}/permission"),
        ]);
        let output = self.run(&args, None, &[0])?;
        repository_permission(&output.stdout, author)
    }

    fn bind_retirement_permissions(
        &self,
        repo: &str,
        mut snapshot: ReviewEvidenceSnapshot,
    ) -> Result<ReviewEvidenceSnapshot, String> {
        let mut authors = BTreeSet::new();
        for event in &snapshot.events {
            let Some(retirement) = retirement_record(&event.body)? else {
                continue;
            };
            if retirement.head_sha == snapshot.head_sha
                && event.state == "ACTIVE"
                && !event.author.is_empty()
            {
                authors.insert(event.author.clone());
            }
        }
        let mut permissions = BTreeMap::new();
        for author in authors {
            let permission = match self.retirement_permission(repo, &author) {
                Ok(permission) => permission,
                Err(error) => {
                    eprintln!(
                        "pr-landing-planner: NOTE: retirement permission unavailable for {author}: {error}; retaining the events without retirement authority"
                    );
                    String::new()
                }
            };
            permissions.insert(author, permission);
        }
        for event in &mut snapshot.events {
            let Some(retirement) = retirement_record(&event.body)? else {
                continue;
            };
            event.retirement_actor_permission = if retirement.head_sha == snapshot.head_sha
                && event.state == "ACTIVE"
                && !event.author.is_empty()
            {
                permissions.get(&event.author).cloned().unwrap_or_default()
            } else {
                String::new()
            };
        }
        Ok(snapshot)
    }

    fn fetch_enrichment(&self, repo: &str, number: i64) -> Result<Option<HostEnrichment>, String> {
        let args = self.net(vec![
            self.gh.clone(),
            "pr".into(),
            "view".into(),
            number.to_string(),
            "--repo".into(),
            repo.into(),
            "--json".into(),
            "number,headRefOid,reviewDecision,statusCheckRollup".into(),
        ]);
        let output = match self.run(&args, None, &[0]) {
            Ok(output) => output,
            Err(_) => return Ok(None),
        };
        let inline_args = self.net(vec![
            self.gh.clone(),
            "api".into(),
            "--paginate".into(),
            "--slurp".into(),
            format!("repos/{repo}/pulls/{number}/comments?per_page=100"),
        ]);
        let inline_output = match self.run(&inline_args, None, &[0]) {
            Ok(output) => output,
            Err(_) => return Ok(None),
        };
        let (review_head, reviews) =
            match self.graphql_connection(repo, number, "reviews", REVIEWS_QUERY) {
                Ok(value) => value,
                Err(_) => return Ok(None),
            };
        let (comment_head, comments) =
            match self.graphql_connection(repo, number, "comments", ISSUE_COMMENTS_QUERY) {
                Ok(value) => value,
                Err(_) => return Ok(None),
            };
        let parsed = (|| -> Result<HostEnrichment, String> {
            let value: Value = if String::from_utf8_lossy(&output.stdout).trim().is_empty() {
                json_object()
            } else {
                serde_json::from_slice(&output.stdout)
                    .map_err(|error| format!("invalid JSON from gh pr view #{number}: {error}"))?
            };
            let obj = value
                .as_object()
                .ok_or_else(|| format!("invalid object from gh pr view #{number}"))?;
            let view_head = string(obj, "headRefOid");
            if view_head.is_empty() || review_head != view_head || comment_head != view_head {
                return Err("PR head changed during review evidence enrichment".to_owned());
            }
            let inline_comments = inline_comments_from_slurp(&inline_output.stdout, number)?;
            let snapshot = review_snapshot(obj, &reviews, &comments, &inline_comments)?;
            let snapshot = self.bind_retirement_permissions(repo, snapshot)?;
            Ok(HostEnrichment {
                checks: parse_rollup(
                    obj.get("statusCheckRollup").unwrap_or(&Value::Null),
                    &snapshot.head_sha,
                ),
                review_snapshot: snapshot,
            })
        })();
        Ok(parsed.ok())
    }

    fn fetch_enrichments(
        &self,
        repo: &str,
        numbers: &[i64],
    ) -> Result<BTreeMap<i64, HostEnrichment>, String> {
        let next = AtomicUsize::new(0);
        let worker_count = numbers.len().min(ENRICHMENT_WORKERS);
        let mut completed = std::thread::scope(|scope| {
            let mut workers = Vec::with_capacity(worker_count);
            for _ in 0..worker_count {
                workers.push(scope.spawn(|| {
                    let mut local = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(number) = numbers.get(index).copied() else {
                            break;
                        };
                        local.push((index, number, self.fetch_enrichment(repo, number)));
                    }
                    local
                }));
            }
            let mut completed = Vec::with_capacity(numbers.len());
            for worker in workers {
                completed.extend(
                    worker
                        .join()
                        .map_err(|_| "rollup collection worker panicked".to_owned())?,
                );
            }
            Ok::<_, String>(completed)
        })?;
        completed.sort_by_key(|(index, _, _)| *index);

        let mut enrichments = BTreeMap::new();
        let mut failed = Vec::new();
        for (_, number, outcome) in completed {
            match outcome? {
                Some(enrichment) => {
                    enrichments.insert(number, enrichment);
                }
                None => failed.push(number),
            }
        }
        if !failed.is_empty() {
            failed.sort_unstable();
            eprintln!(
                "pr-landing-planner: NOTE: check/review evidence enrichment failed for {} PR(s) ({}); treating checks as pending and review-resolution authority as unavailable",
                failed.len(),
                failed.iter().map(|number| format!("#{number}")).collect::<Vec<_>>().join(",")
            );
        }
        Ok(enrichments)
    }
}

fn json_object() -> Value {
    Value::Object(Map::new())
}

fn repository_permission(bytes: &[u8], expected_author: &str) -> Result<String, String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid repository permission JSON: {error}"))?;
    let response = value
        .as_object()
        .ok_or("repository permission response is not an object")?;
    let user = response
        .get("user")
        .and_then(Value::as_object)
        .ok_or("repository permission response lacks a user object")?;
    let actual_actor = user.get("login").and_then(Value::as_str).unwrap_or("");
    if actual_actor.is_empty() || !actual_actor.eq_ignore_ascii_case(expected_author) {
        return Err(
            "repository permission response actor differs from GitHub event author".to_owned(),
        );
    }
    for field in ["role_name", "permission"] {
        let Some(candidate) = response.get(field).and_then(Value::as_str) else {
            continue;
        };
        let candidate = candidate.to_ascii_lowercase();
        if ALLOWED_RETIREMENT_PERMISSIONS.contains(&candidate.as_str()) {
            return Ok(candidate);
        }
    }
    Err("GitHub event author lacks current triage-or-higher repository permission".to_owned())
}

fn string(obj: &Map<String, Value>, key: &str) -> String {
    match obj.get(key) {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        Some(Value::Bool(value)) => value.to_string(),
        _ => String::new(),
    }
}

fn integer(obj: &Map<String, Value>, key: &str) -> i64 {
    obj.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn labels(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter_map(|entry| entry.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn event_object<'a>(value: &'a Value, role: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{role} is not an object"))
}

fn stable_identity(event: &Map<String, Value>, role: &str) -> Result<String, String> {
    let identity = event.get("id").and_then(Value::as_str).unwrap_or("");
    if identity.is_empty() {
        return Err(format!("{role} lacks a stable id"));
    }
    Ok(identity.to_owned())
}

fn required_string(event: &Map<String, Value>, role: &str, key: &str) -> Result<String, String> {
    let value = event.get(key).and_then(Value::as_str).unwrap_or("");
    if value.is_empty() {
        return Err(format!("{role} lacks a non-empty string {key}"));
    }
    Ok(value.to_owned())
}

fn event_author(event: &Map<String, Value>, role: &str, key: &str) -> Result<String, String> {
    let Some(value) = event.get(key) else {
        return Ok(String::new());
    };
    let Some(author) = value.as_object() else {
        return if value.is_null() {
            Ok(String::new())
        } else {
            Err(format!("{role} author is not an object or null"))
        };
    };
    match author.get("login") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(login)) => Ok(login.trim().to_owned()),
        Some(_) => Err(format!("{role} author login is not a string or null")),
    }
}

fn numeric_identity(event: &Map<String, Value>, role: &str) -> Result<String, String> {
    let identity = event.get("id").and_then(Value::as_u64).unwrap_or(0);
    if identity == 0 {
        return Err(format!("{role} lacks a stable positive numeric id"));
    }
    Ok(identity.to_string())
}

fn required_nullable_string(
    event: &Map<String, Value>,
    role: &str,
    key: &str,
) -> Result<String, String> {
    match event.get(key) {
        None => Err(format!("{role} lacks promised field {key}")),
        Some(Value::Null) => Ok(String::new()),
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        Some(_) => Err(format!(
            "{role} field {key} is not a non-empty string or null"
        )),
    }
}

fn optional_head(event: &Map<String, Value>, role: &str) -> Result<String, String> {
    match event.get("commit_id") {
        None => Err(format!("{role} lacks promised commit_id")),
        Some(Value::Null) => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{role} commit_id is not a string or null")),
    }
}

fn inline_state(event: &Map<String, Value>, role: &str) -> Result<String, String> {
    let mut state = BTreeMap::new();
    for key in INLINE_STATE_FIELDS {
        let value = event
            .get(key)
            .cloned()
            .ok_or_else(|| format!("{role} lacks promised field {key}"))?;
        let valid = match &value {
            Value::Null | Value::String(_) => true,
            Value::Number(number) => number.as_i64().is_some() || number.as_u64().is_some(),
            _ => false,
        };
        if !valid {
            return Err(format!(
                "{role} field {key} is not a string, integer, or null"
            ));
        }
        state.insert(key, value);
    }
    if state
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        return Err(format!("{role} lacks a non-empty path"));
    }
    serde_json::to_string(&state).map_err(|error| format!("cannot encode {role} state: {error}"))
}

fn inline_comments_from_slurp(bytes: &[u8], number: i64) -> Result<Vec<Value>, String> {
    let value: Value = if String::from_utf8_lossy(bytes).trim().is_empty() {
        Value::Array(Vec::new())
    } else {
        serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid inline review-comment JSON for #{number}: {error}"))?
    };
    let pages = value
        .as_array()
        .ok_or("inline review-comment pagination is not an array")?;
    if pages.is_empty() {
        return Err("inline review-comment pagination is empty".to_owned());
    }
    let mut flattened = Vec::new();
    for (page_index, page) in pages.iter().enumerate() {
        let entries = page
            .as_array()
            .ok_or_else(|| format!("inline review-comment page {page_index} is not an array"))?;
        flattened.extend(entries.iter().cloned());
    }
    Ok(flattened)
}

fn graphql_connection_from_slurp(
    bytes: &[u8],
    number: i64,
    connection: &str,
) -> Result<(String, Vec<Value>), String> {
    let value: Value = if String::from_utf8_lossy(bytes).trim().is_empty() {
        Value::Array(Vec::new())
    } else {
        serde_json::from_slice(bytes).map_err(|error| {
            format!("invalid {connection} pagination JSON for #{number}: {error}")
        })?
    };
    let pages = value.as_array().ok_or_else(|| {
        format!("{connection} pagination for PR #{number} is not a non-empty array")
    })?;
    if pages.is_empty() {
        return Err(format!(
            "{connection} pagination for PR #{number} is not a non-empty array"
        ));
    }
    let mut head = String::new();
    let mut flattened = Vec::new();
    for (page_index, page_value) in pages.iter().enumerate() {
        let page = event_object(page_value, &format!("{connection} page[{page_index}]"))?;
        if let Some(errors) = page.get("errors") {
            let errors = errors.as_array().ok_or_else(|| {
                format!("{connection} page {page_index} GraphQL errors is not an array")
            })?;
            if !errors.is_empty() {
                return Err(format!(
                    "{connection} page {page_index} contains GraphQL errors"
                ));
            }
        }
        let data = event_object(
            page.get("data").unwrap_or(&Value::Null),
            &format!("{connection} page[{page_index}].data"),
        )?;
        let repository = event_object(
            data.get("repository").unwrap_or(&Value::Null),
            &format!("{connection} page[{page_index}].repository"),
        )?;
        let pull_request = event_object(
            repository.get("pullRequest").unwrap_or(&Value::Null),
            &format!("{connection} page[{page_index}].pullRequest"),
        )?;
        let page_head = required_string(
            pull_request,
            &format!("{connection} page {page_index}"),
            "headRefOid",
        )?;
        if !head.is_empty() && page_head != head {
            return Err(format!(
                "{connection} PR head changed during pagination: {head} -> {page_head}"
            ));
        }
        head = page_head;
        let connection_value = event_object(
            pull_request.get(connection).unwrap_or(&Value::Null),
            &format!("{connection} page[{page_index}].{connection}"),
        )?;
        let nodes = connection_value
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{connection} page {page_index} lacks nodes"))?;
        let page_info = connection_value
            .get("pageInfo")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("{connection} page {page_index} lacks pageInfo"))?;
        let has_next = page_info
            .get("hasNextPage")
            .and_then(Value::as_bool)
            .ok_or_else(|| format!("{connection} page {page_index} lacks hasNextPage"))?;
        if has_next
            && page_info
                .get("endCursor")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
        {
            return Err(format!(
                "{connection} page {page_index} lacks a next-page cursor"
            ));
        }
        if page_index + 1 < pages.len() && !has_next {
            return Err(format!(
                "{connection} pagination continued after its terminal page"
            ));
        }
        if page_index + 1 == pages.len() && has_next {
            return Err(format!(
                "{connection} pagination ended before its terminal page"
            ));
        }
        flattened.extend(nodes.iter().cloned());
    }
    Ok((head, flattened))
}

fn event_body(event: &Map<String, Value>, role: &str) -> Result<String, String> {
    match event.get("body") {
        Some(Value::String(body)) => Ok(body.clone()),
        _ => Err(format!("{role} lacks a string body")),
    }
}

fn review_snapshot(
    obj: &Map<String, Value>,
    reviews: &[Value],
    comments: &[Value],
    inline_comments: &[Value],
) -> Result<ReviewEvidenceSnapshot, String> {
    let head = required_string(obj, "review snapshot", "headRefOid")?;
    if !obj.contains_key("reviewDecision") {
        return Err("review snapshot lacks promised reviewDecision".to_owned());
    }
    let decision = match obj.get("reviewDecision") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(value)) => value.clone(),
        Some(_) => return Err("review snapshot decision is not a string or null".to_owned()),
    };
    let mut events = Vec::with_capacity(reviews.len() + comments.len() + inline_comments.len());
    for (index, value) in reviews.iter().enumerate() {
        let role = format!("review[{index}]");
        let event = event_object(value, &role)?;
        let state = required_string(event, &role, "state")?;
        if !NATIVE_REVIEW_STATES.contains(&state.as_str()) {
            return Err(format!("{role} has unknown state {state:?}"));
        }
        let commit = event
            .get("commit")
            .ok_or_else(|| format!("{role} lacks a commit"))?;
        let event_head = required_string(
            event_object(commit, &format!("{role}.commit"))?,
            &format!("{role}.commit"),
            "oid",
        )?;
        events.push(ReviewEvidenceEvent {
            kind: "review".to_owned(),
            identity: stable_identity(event, &role)?,
            author: event_author(event, &role, "author")?,
            state,
            head_sha: event_head,
            created_at: required_string(event, &role, "submittedAt")?,
            updated_at: required_string(event, &role, "updatedAt")?,
            last_edited_at: required_nullable_string(event, &role, "lastEditedAt")?,
            body: event_body(event, &role)?,
            retirement_actor_permission: String::new(),
        });
    }
    for (index, value) in comments.iter().enumerate() {
        let role = format!("issue-comment[{index}]");
        let event = event_object(value, &role)?;
        let minimized = match event.get("isMinimized") {
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(format!("{role} isMinimized is not a boolean")),
            None => return Err(format!("{role} lacks isMinimized")),
        };
        let reason = match event.get("minimizedReason") {
            None => return Err(format!("{role} lacks promised minimizedReason")),
            Some(Value::Null) => String::new(),
            Some(Value::String(value)) => value.clone(),
            Some(_) => return Err(format!("{role} minimizedReason is not a string or null")),
        };
        if minimized && reason.is_empty() {
            return Err(format!("{role} is minimized without a reason"));
        }
        events.push(ReviewEvidenceEvent {
            kind: "issue-comment".to_owned(),
            identity: stable_identity(event, &role)?,
            author: event_author(event, &role, "author")?,
            state: if minimized {
                format!("MINIMIZED:{reason}")
            } else {
                "ACTIVE".to_owned()
            },
            // Issue comments have no commit identity. Empty is the canonical absent value;
            // the enclosing evidence snapshot remains bound to the exact PR head.
            head_sha: String::new(),
            created_at: required_string(event, &role, "createdAt")?,
            updated_at: required_string(event, &role, "updatedAt")?,
            last_edited_at: String::new(),
            body: event_body(event, &role)?,
            retirement_actor_permission: String::new(),
        });
    }
    for (index, value) in inline_comments.iter().enumerate() {
        let role = format!("review-comment[{index}]");
        let event = event_object(value, &role)?;
        events.push(ReviewEvidenceEvent {
            kind: "review-comment".to_owned(),
            identity: numeric_identity(event, &role)?,
            author: event_author(event, &role, "user")?,
            state: inline_state(event, &role)?,
            // REST review comments may omit commit_id. Preserve that absence rather than
            // fabricating the enclosing snapshot head.
            head_sha: optional_head(event, &role)?,
            created_at: required_string(event, &role, "created_at")?,
            updated_at: required_string(event, &role, "updated_at")?,
            last_edited_at: String::new(),
            body: event_body(event, &role)?,
            retirement_actor_permission: String::new(),
        });
    }
    Ok(ReviewEvidenceSnapshot {
        head_sha: head,
        review_decision: decision,
        events,
    })
}
fn light_pr_entries(bytes: &[u8]) -> Result<Vec<Map<String, Value>>, String> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(
            "open PR list returned empty stdout; review evidence is unavailable".to_owned(),
        );
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        format!("open PR list returned invalid JSON; review evidence is unavailable: {error}")
    })?;
    let entries = value
        .as_array()
        .ok_or("open PR list response is not an array; review evidence is unavailable")?;
    if entries.len() >= PR_LIST_LIMIT {
        return Err(format!(
            "open PR list reached the {PR_LIST_LIMIT}-item limit; review evidence may be incomplete"
        ));
    }
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            entry.as_object().cloned().ok_or_else(|| {
                format!(
                    "open PR list entry {index} is not an object; review evidence is unavailable"
                )
            })
        })
        .collect()
}

fn raw_pr_from_list_entry(obj: &Map<String, Value>, enrichment: Option<&HostEnrichment>) -> RawPr {
    let author = obj
        .get("author")
        .and_then(Value::as_object)
        .and_then(|author| author.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    RawPr {
        number: integer(obj, "number"),
        head_ref: string(obj, "headRefName"),
        base_ref: string(obj, "baseRefName"),
        api_head_sha: string(obj, "headRefOid"),
        title: string(obj, "title"),
        author,
        is_draft: obj.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        mergeable: string(obj, "mergeable"),
        // Preserve the independently observed light-list decision. Collection
        // compares it with the later evidence snapshot instead of silently
        // replacing a non-empty decision with missing or contradictory data.
        review_decision: string(obj, "reviewDecision"),
        review_evidence_unavailable: enrichment.is_none(),
        created_at: string(obj, "createdAt"),
        updated_at: string(obj, "updatedAt"),
        additions: integer(obj, "additions"),
        deletions: integer(obj, "deletions"),
        labels: labels(obj.get("labels")),
        checks: enrichment
            .map(|value| value.checks.clone())
            .unwrap_or_default(),
        review_snapshot: enrichment.map(|value| value.review_snapshot.clone()),
        mechanism_symbols: Vec::new(),
    }
}

impl VcsHost for GitHubHost {
    fn list_open_prs(&mut self, repo: &str, _base: Option<&str>) -> Result<Vec<RawPr>, String> {
        let args = self.net(vec![
            self.gh.clone(),
            "pr".into(),
            "list".into(),
            "--repo".into(),
            repo.into(),
            "--state".into(),
            "open".into(),
            "--limit".into(),
            PR_LIST_LIMIT.to_string(),
            "--json".into(),
            LIGHT_FIELDS.join(","),
        ]);
        let output = self.run(&args, None, &[0])?;
        let entries = light_pr_entries(&output.stdout)?;
        let numbers = entries
            .iter()
            .map(|obj| integer(obj, "number"))
            .collect::<Vec<_>>();
        let enrichments = self.fetch_enrichments(repo, &numbers)?;
        let mut prs = Vec::new();
        for obj in &entries {
            prs.push(raw_pr_from_list_entry(
                obj,
                enrichments.get(&integer(obj, "number")),
            ));
        }
        Ok(prs)
    }

    fn prefetch_refs(
        &mut self,
        refspecs: &[(String, String)],
    ) -> Result<BTreeMap<String, String>, String> {
        if refspecs.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut command = vec![
            "git".into(),
            "fetch".into(),
            "--quiet".into(),
            "--no-tags".into(),
            self.remote.clone(),
        ];
        command.extend(
            refspecs
                .iter()
                .map(|(source, dest)| format!("+{source}:{dest}")),
        );
        let args = self.net(command);
        self.run(&args, Some(&self.git_dir), &[0])?;
        let mut resolved = BTreeMap::new();
        for (_, dest) in refspecs {
            let args = vec!["git".into(), "rev-parse".into(), dest.clone()];
            let output = self.run(&args, Some(&self.git_dir), &[0])?;
            resolved.insert(
                dest.clone(),
                String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            );
        }
        Ok(resolved)
    }

    fn merge_tree(&mut self, left: &str, right: &str) -> Result<Vec<String>, String> {
        let args = vec![
            "git".into(),
            "merge-tree".into(),
            "--write-tree".into(),
            "--name-only".into(),
            "--messages".into(),
            left.into(),
            right.into(),
        ];
        let output = self.run(&args, Some(&self.git_dir), &[0, 1])?;
        if output.status.success() {
            return Ok(Vec::new());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut paths = stdout
            .lines()
            .skip(1)
            .take_while(|line| !line.trim().is_empty())
            .map(|line| line.trim().to_owned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        paths.sort();
        Ok(paths)
    }

    fn is_ancestor(&mut self, ancestor: &str, descendant: &str) -> Result<bool, String> {
        let args = vec![
            "git".into(),
            "merge-base".into(),
            "--is-ancestor".into(),
            ancestor.into(),
            descendant.into(),
        ];
        Ok(self
            .run(&args, Some(&self.git_dir), &[0, 1])?
            .status
            .success())
    }

    fn changed_files(
        &mut self,
        base_sha: &str,
        head_sha: &str,
    ) -> Result<BTreeSet<String>, String> {
        let args = vec![
            "git".into(),
            "merge-base".into(),
            base_sha.into(),
            head_sha.into(),
        ];
        let output = self.run(&args, Some(&self.git_dir), &[0])?;
        let merge_base = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let args = vec![
            "git".into(),
            "diff".into(),
            "--name-only".into(),
            format!("{merge_base}...{head_sha}"),
        ];
        let output = self.run(&args, Some(&self.git_dir), &[0])?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }

    fn commits_behind(&mut self, head_sha: &str, base_sha: &str) -> Result<i64, String> {
        let args = vec![
            "git".into(),
            "rev-list".into(),
            "--count".into(),
            format!("{head_sha}..{base_sha}"),
        ];
        let output = self.run(&args, Some(&self.git_dir), &[0])?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::{
        graphql_connection_from_slurp, inline_comments_from_slurp, light_pr_entries,
        raw_pr_from_list_entry, repository_permission, review_snapshot, ISSUE_COMMENTS_QUERY,
        PR_LIST_LIMIT, REVIEWS_QUERY,
    };
    use crate::context::review_evidence_digest;

    fn review_view() -> serde_json::Value {
        json!({
            "headRefOid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "reviewDecision": "CHANGES_REQUESTED",
            "reviews": [{
                "id": "review-1",
                "author": {"login": "reviewer"},
                "state": "CHANGES_REQUESTED",
                "commit": {"oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                "submittedAt": "2026-09-04T12:00:00Z",
                "updatedAt": "2026-09-04T12:00:00Z",
                "lastEditedAt": null,
                "body": "please fix"
            }],
            "comments": [{
                "id": "issue-comment-1",
                "author": {"login": "release-authority"},
                "isMinimized": false,
                "minimizedReason": null,
                "createdAt": "2026-09-04T12:00:00Z",
                "updatedAt": "2026-09-04T12:00:00Z",
                "body": "tracked objection"
            }]
        })
    }

    fn inline(position: serde_json::Value) -> serde_json::Value {
        json!({
            "id": 987,
            "user": {"login": "reviewer"},
            "pull_request_review_id": 654,
            "in_reply_to_id": null,
            "body": "inline objection",
            "path": "src/lib.rs",
            "position": position,
            "original_position": 7,
            "line": position,
            "original_line": 7,
            "original_start_line": null,
            "side": "RIGHT",
            "start_line": null,
            "start_side": null,
            "subject_type": "line",
            "commit_id": null,
            "original_commit_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "created_at": "2026-09-04T12:00:00Z",
            "updated_at": "2026-09-04T12:00:00Z"
        })
    }

    #[test]
    fn paginated_inline_comments_are_flattened_without_dropping_pages() {
        let raw = serde_json::to_vec(&json!([[inline(json!(7))], [inline(Value::Null)]])).unwrap();
        let comments = inline_comments_from_slurp(&raw, 42).unwrap();
        assert_eq!(comments.len(), 2);
    }

    #[test]
    fn paginated_graphql_connection_includes_every_page() {
        assert!(REVIEWS_QUERY.contains("submittedAt updatedAt lastEditedAt"));
        assert!(ISSUE_COMMENTS_QUERY.contains("createdAt updatedAt"));
        let page = |id: &str, has_next: bool, cursor: Value| {
            json!({"data":{"repository":{"pullRequest":{
                "headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "reviews":{
                    "nodes":[{"id":id}],
                    "pageInfo":{"hasNextPage":has_next,"endCursor":cursor}
                }
            }}}})
        };
        let raw = serde_json::to_vec(&json!([
            page("review-1", true, json!("cursor-1")),
            page("review-2", false, Value::Null)
        ]))
        .unwrap();
        let (head, reviews) = graphql_connection_from_slurp(&raw, 42, "reviews").unwrap();
        assert_eq!(head, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(reviews.len(), 2);
    }

    #[test]
    fn failed_enrichment_is_explicit_in_the_production_list_record() {
        let value = json!({
            "number": 42,
            "headRefName": "feature",
            "baseRefName": "main",
            "headRefOid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "title": "title",
            "author": {"login": "author"},
            "isDraft": false,
            "mergeable": "MERGEABLE",
            "reviewDecision": "APPROVED",
            "createdAt": "2026-09-05T12:00:00Z",
            "updatedAt": "2026-09-05T12:00:00Z",
            "additions": 1,
            "deletions": 1,
            "labels": []
        });
        let raw = raw_pr_from_list_entry(value.as_object().unwrap(), None);
        assert!(raw.review_evidence_unavailable);
        assert!(raw.review_snapshot.is_none());
        assert!(raw.checks.is_empty());
        assert_eq!(raw.review_decision, "APPROVED");
    }
    #[test]
    fn production_light_list_refuses_unknown_availability() {
        assert!(light_pr_entries(b"[]").unwrap().is_empty());
        assert!(light_pr_entries(b" \t\r\n")
            .unwrap_err()
            .contains("empty stdout"));

        let malformed = serde_json::to_vec(&json!([{"number": 1}, "not-an-object"])).unwrap();
        assert!(light_pr_entries(&malformed)
            .unwrap_err()
            .contains("entry 1 is not an object"));

        let capped = serde_json::to_vec(&vec![json!({}); PR_LIST_LIMIT]).unwrap();
        assert!(light_pr_entries(&capped)
            .unwrap_err()
            .contains("500-item limit"));
    }

    #[test]
    fn incomplete_graphql_connection_fails_closed() {
        let raw = serde_json::to_vec(&json!([{"data":{"repository":{"pullRequest":{
            "headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "reviews":{
                "nodes":[],
                "pageInfo":{"hasNextPage":true,"endCursor":"cursor-1"}
            }
        }}}}]))
        .unwrap();
        assert!(graphql_connection_from_slurp(&raw, 42, "reviews")
            .unwrap_err()
            .contains("ended before its terminal page"));
    }

    #[test]
    fn graphql_partial_data_with_errors_fails_closed() {
        let raw = serde_json::to_vec(&json!([{
            "errors":[{"message":"review evidence is incomplete"}],
            "data":{"repository":{"pullRequest":{
                "headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "reviews":{
                    "nodes":[],
                    "pageInfo":{"hasNextPage":false,"endCursor":null}
                }
            }}}
        }]))
        .unwrap();
        assert!(graphql_connection_from_slurp(&raw, 42, "reviews")
            .unwrap_err()
            .contains("contains GraphQL errors"));
    }

    #[test]
    fn repository_permission_requires_matching_actor_and_triage_or_higher() {
        for permission in ["triage", "write", "maintain", "admin"] {
            let raw = serde_json::to_vec(&json!({
                "permission": permission,
                "role_name": permission,
                "user": {"login": "release-authority"}
            }))
            .unwrap();
            assert_eq!(
                repository_permission(&raw, "release-authority").unwrap(),
                permission
            );
        }
        let triage = serde_json::to_vec(&json!({
            "permission": "read",
            "role_name": "triage",
            "user": {"login": "release-authority"}
        }))
        .unwrap();
        assert_eq!(
            repository_permission(&triage, "release-authority").unwrap(),
            "triage"
        );
        let read = serde_json::to_vec(&json!({
            "permission": "read",
            "role_name": "read",
            "user": {"login": "release-authority"}
        }))
        .unwrap();
        assert!(repository_permission(&read, "release-authority")
            .unwrap_err()
            .contains("triage-or-higher"));
        let missing = serde_json::to_vec(&json!({
            "user": {"login": "release-authority"}
        }))
        .unwrap();
        assert!(repository_permission(&missing, "release-authority")
            .unwrap_err()
            .contains("triage-or-higher"));
        let mismatch = serde_json::to_vec(&json!({
            "permission": "write",
            "role_name": "write",
            "user": {"login": "different-actor"}
        }))
        .unwrap();
        assert!(repository_permission(&mismatch, "release-authority")
            .unwrap_err()
            .contains("differs from GitHub event author"));
        assert!(repository_permission(b"{}", "release-authority")
            .unwrap_err()
            .contains("user object"));
    }

    #[test]
    fn production_snapshot_binds_same_timestamp_inline_retirement() {
        let view = review_view();
        let obj = view.as_object().unwrap();
        let reviews = obj["reviews"].as_array().unwrap();
        let comments = obj["comments"].as_array().unwrap();
        let active = review_snapshot(obj, reviews, comments, &[inline(json!(7))]).unwrap();
        let retired = review_snapshot(obj, reviews, comments, &[inline(Value::Null)]).unwrap();

        assert_eq!(active.events.len(), 3);
        assert!(active
            .events
            .iter()
            .filter(|event| event.kind != "review")
            .all(|event| event.head_sha.is_empty()));
        assert_ne!(
            review_evidence_digest(&active).unwrap(),
            review_evidence_digest(&retired).unwrap()
        );
    }

    #[test]
    fn production_snapshot_rejects_missing_inline_identity() {
        let view = review_view();
        let mut comment = inline(json!(7));
        comment.as_object_mut().unwrap().remove("id");
        let obj = view.as_object().unwrap();
        assert!(review_snapshot(
            obj,
            obj["reviews"].as_array().unwrap(),
            obj["comments"].as_array().unwrap(),
            &[comment]
        )
        .unwrap_err()
        .contains("stable positive numeric id"));
    }

    #[test]
    fn production_snapshot_rejects_missing_authority_fields() {
        let view = review_view();
        let obj = view.as_object().unwrap();
        let reviews = obj["reviews"].as_array().unwrap();
        let comments = obj["comments"].as_array().unwrap();
        let inline_comments = vec![inline(json!(7))];

        let mut missing_decision = obj.clone();
        missing_decision.remove("reviewDecision");
        assert!(
            review_snapshot(&missing_decision, reviews, comments, &inline_comments)
                .unwrap_err()
                .contains("reviewDecision")
        );

        let mut alien_reviews = reviews.clone();
        alien_reviews[0]
            .as_object_mut()
            .unwrap()
            .insert("state".to_owned(), Value::String("ALIEN_STATE".to_owned()));
        assert!(
            review_snapshot(obj, &alien_reviews, comments, &inline_comments)
                .unwrap_err()
                .contains("unknown state")
        );

        let mut bad_reviews = reviews.clone();
        bad_reviews[0].as_object_mut().unwrap().remove("updatedAt");
        assert!(
            review_snapshot(obj, &bad_reviews, comments, &inline_comments)
                .unwrap_err()
                .contains("updatedAt")
        );

        let mut bad_comments = comments.clone();
        bad_comments[0].as_object_mut().unwrap().remove("createdAt");
        assert!(
            review_snapshot(obj, reviews, &bad_comments, &inline_comments)
                .unwrap_err()
                .contains("createdAt")
        );
        let mut anonymous_reviews = reviews.clone();
        anonymous_reviews[0]
            .as_object_mut()
            .unwrap()
            .insert("author".to_owned(), Value::Null);
        let anonymous = review_snapshot(obj, &anonymous_reviews, comments, &inline_comments)
            .expect("missing event author remains visible");
        assert!(anonymous.events[0].author.is_empty());
        assert!(review_evidence_digest(&anonymous).is_ok());

        let mut bad_inline = inline_comments;
        bad_inline[0].as_object_mut().unwrap().remove("created_at");
        assert!(review_snapshot(obj, reviews, comments, &bad_inline)
            .unwrap_err()
            .contains("created_at"));
    }
}
