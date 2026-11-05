//! Content guard for WRITE requests: refuse oversized bodies and base64-looking payloads.
//!
//! GitHub issue, comment and pull-request text is for short human-readable notes. This guard
//! refuses a write whose body sources (inline flags, body files, `gh api` fields, `--input`
//! request files, and stdin) total more than `max_body_bytes`, or contain a base64-looking run
//! longer than `max_base64_run` characters. It also refuses write forms whose body gh composes
//! itself after the guard has run (an editor, a template, `--fill`, an interactive prompt),
//! because their content cannot be inspected. `GH_PACED_ALLOW_LARGE_BODY=1` skips it.

use crate::classify::{normalize_endpoint, Classification};
use crate::config::Config;
use std::io::Read;

/// How a body source supplies its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// The flag value itself is the text.
    Inline(String),
    /// The text is in this file.
    File(String),
    /// The text arrives on stdin.
    Stdin,
}

/// Where a file path sits in the argument list: `rest[index] == prefix + path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgLocation {
    /// Index into the argument list after the command words.
    pub index: usize,
    /// Text before the path in that argument (`--body-file=`, `-F`, `body=@`, or empty).
    pub prefix: String,
}

/// One place a write request takes body text from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodySource {
    /// The flag (or `<file>` / `<arg>` for a positional) that names this source.
    pub flag: String,
    /// Where the content comes from.
    pub kind: SourceKind,
    /// The content is a JSON request body (`gh api --input`, `workflow run --json`, `--recover`).
    pub json: bool,
    /// For a file source, where its path is in the argument list (used to substitute a snapshot).
    pub location: Option<ArgLocation>,
    /// Sources with the same key override each other (gh keeps the last); `None` for repeatable
    /// sources such as positional files.
    key: Option<String>,
}

impl BodySource {
    /// True when this source reads stdin.
    pub fn is_stdin(&self) -> bool {
        self.kind == SourceKind::Stdin
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Inline text.
    Text,
    /// A path, `-` = stdin.
    File,
    /// `key=value`, value inline.
    RawField,
    /// `key=value`, value `@path`, `@-` (stdin) or inline.
    TypedField,
    /// A JSON request body file, `-` = stdin.
    Input,
    /// A boolean flag meaning "the JSON body is on stdin".
    JsonStdin,
}

struct Table {
    /// Body-carrying long flags.
    long: &'static [(&'static str, Kind)],
    /// Body-carrying short flags, each mapped to its long name in `long`.
    short: &'static [(char, &'static str)],
    /// Other long flags that take a value (skipped).
    value_long: &'static [&'static str],
    /// Other short flags that take a value (skipped).
    value_short: &'static str,
    /// Positional arguments are files to upload (`gist create`).
    positional_files: bool,
    /// Positional arguments are text of unknown meaning (aliases, extensions, other writes).
    positional_text: bool,
    /// stdin carries the value when no flag does and stdin is not a terminal (`secret set`).
    stdin_when_no_body: bool,
}

const API: Table = Table {
    long: &[
        ("raw-field", Kind::RawField),
        ("field", Kind::TypedField),
        ("input", Kind::Input),
    ],
    short: &[('f', "raw-field"), ('F', "field")],
    value_long: &[
        "method", "header", "jq", "template", "preview", "cache", "hostname",
    ],
    value_short: "XHqtp",
    positional_files: false,
    positional_text: false,
    stdin_when_no_body: false,
};

const WORKFLOW_RUN: Table = Table {
    long: &[
        ("raw-field", Kind::RawField),
        ("field", Kind::TypedField),
        ("json", Kind::JsonStdin),
    ],
    short: &[('f', "raw-field"), ('F', "field")],
    value_long: &["ref", "repo"],
    value_short: "rR",
    positional_files: false,
    positional_text: false,
    stdin_when_no_body: false,
};

const CREATE: Table = Table {
    long: &[
        ("title", Kind::Text),
        ("body", Kind::Text),
        ("body-file", Kind::File),
        ("recover", Kind::Input),
    ],
    short: &[('t', "title"), ('b', "body"), ('F', "body-file")],
    value_long: &[
        "base",
        "head",
        "assignee",
        "label",
        "milestone",
        "project",
        "reviewer",
        "repo",
        "template",
    ],
    value_short: "BHalmprRT",
    positional_files: false,
    positional_text: false,
    stdin_when_no_body: false,
};

const COMMENT: Table = Table {
    long: &[("body", Kind::Text), ("body-file", Kind::File)],
    short: &[('b', "body"), ('F', "body-file")],
    value_long: &["repo"],
    value_short: "R",
    positional_files: false,
    positional_text: false,
    stdin_when_no_body: false,
};

const EDIT: Table = Table {
    long: &[
        ("title", Kind::Text),
        ("body", Kind::Text),
        ("body-file", Kind::File),
    ],
    short: &[('t', "title"), ('b', "body"), ('F', "body-file")],
    value_long: &[
        "base",
        "milestone",
        "add-assignee",
        "remove-assignee",
        "add-label",
        "remove-label",
        "add-project",
        "remove-project",
        "add-reviewer",
        "remove-reviewer",
        "repo",
    ],
    value_short: "BmR",
    positional_files: false,
    positional_text: false,
    stdin_when_no_body: false,
};

const MERGE: Table = Table {
    long: &[
        ("body", Kind::Text),
        ("body-file", Kind::File),
        ("subject", Kind::Text),
    ],
    short: &[('b', "body"), ('F', "body-file"), ('t', "subject")],
    value_long: &["author-email", "match-head-commit", "repo"],
    value_short: "AR",
    positional_files: false,
    positional_text: false,
    stdin_when_no_body: false,
};

const CLOSE: Table = Table {
    long: &[("comment", Kind::Text)],
    short: &[('c', "comment")],
    value_long: &["reason", "repo"],
    value_short: "rR",
    positional_files: false,
    positional_text: false,
    stdin_when_no_body: false,
};

const RELEASE: Table = Table {
    long: &[
        ("notes", Kind::Text),
        ("notes-file", Kind::File),
        ("title", Kind::Text),
    ],
    short: &[('n', "notes"), ('F', "notes-file"), ('t', "title")],
    value_long: &[
        "target",
        "discussion-category",
        "notes-start-tag",
        "tag",
        "repo",
    ],
    value_short: "R",
    positional_files: false,
    positional_text: false,
    stdin_when_no_body: false,
};

const GIST_CREATE: Table = Table {
    long: &[("desc", Kind::Text)],
    short: &[('d', "desc")],
    value_long: &["filename"],
    value_short: "f",
    positional_files: true,
    positional_text: false,
    stdin_when_no_body: false,
};

const GIST_EDIT: Table = Table {
    long: &[("desc", Kind::Text), ("add", Kind::File)],
    short: &[('d', "desc"), ('a', "add")],
    value_long: &["filename", "remove"],
    value_short: "fr",
    positional_files: false,
    positional_text: false,
    stdin_when_no_body: false,
};

const LABEL: Table = Table {
    long: &[("description", Kind::Text), ("name", Kind::Text)],
    short: &[('d', "description"), ('n', "name")],
    value_long: &["color", "repo"],
    value_short: "cR",
    positional_files: false,
    positional_text: false,
    stdin_when_no_body: false,
};

const REPO: Table = Table {
    long: &[("description", Kind::Text)],
    short: &[('d', "description")],
    value_long: &[
        "homepage",
        "team",
        "template",
        "gitignore",
        "license",
        "source",
        "remote",
    ],
    value_short: "hgltpsr",
    positional_files: false,
    positional_text: false,
    stdin_when_no_body: false,
};

const SECRET: Table = Table {
    long: &[("body", Kind::Text), ("env-file", Kind::File)],
    short: &[('b', "body"), ('f', "env-file")],
    value_long: &["app", "env", "org", "visibility", "repos", "repo"],
    value_short: "aeovrRu",
    positional_files: false,
    positional_text: false,
    stdin_when_no_body: true,
};

/// Any other write, including aliases and extensions: the common body flags, and every
/// positional argument counted as inline text (an alias may turn one into a comment body).
const GENERIC: Table = Table {
    long: &[
        ("body", Kind::Text),
        ("body-file", Kind::File),
        ("input", Kind::Input),
    ],
    short: &[('b', "body")],
    value_long: &["repo"],
    value_short: "R",
    positional_files: false,
    positional_text: true,
    stdin_when_no_body: false,
};

fn table_for(c: &Classification) -> &'static Table {
    if c.opaque {
        // An alias beneath a group (`issue publish`) or a passthrough command: the family's own
        // flag table says nothing about these arguments.
        return &GENERIC;
    }
    match (c.family.as_str(), c.sub.as_deref()) {
        ("api", _) => &API,
        ("workflow", Some("run")) => &WORKFLOW_RUN,
        ("pr" | "issue", Some("create")) => &CREATE,
        ("pr" | "issue", Some("comment")) | ("pr", Some("review")) => &COMMENT,
        ("pr" | "issue", Some("edit")) => &EDIT,
        ("pr", Some("merge")) => &MERGE,
        ("pr" | "issue", Some("close" | "reopen")) => &CLOSE,
        ("release", Some("create" | "edit")) => &RELEASE,
        ("gist", Some("create")) => &GIST_CREATE,
        ("gist", Some("edit")) => &GIST_EDIT,
        ("label", Some("create" | "edit")) => &LABEL,
        ("repo", Some("create" | "edit")) => &REPO,
        ("secret" | "variable", Some("set")) => &SECRET,
        _ => &GENERIC,
    }
}

/// Location of `path` at the end of `rest[index]`.
fn location(rest: &[String], index: usize, path: &str) -> Option<ArgLocation> {
    let token = rest.get(index)?;
    let cut = token.len().checked_sub(path.len())?;
    if !token.ends_with(path) {
        return None;
    }
    Some(ArgLocation {
        index,
        prefix: token[..cut].to_string(),
    })
}

/// Record one body source. `name` is the canonical long name, `index` the argument holding
/// `value` (so file paths can be located for snapshot substitution).
fn push_source(
    out: &mut Vec<BodySource>,
    rest: &[String],
    name: &str,
    kind: Kind,
    value: &str,
    index: usize,
) {
    let flag = format!("--{name}");
    let scalar_key = Some(name.to_string());
    let field_key = |v: &str| -> Option<String> {
        let key = v.split_once('=').map(|(k, _)| k).unwrap_or(v);
        // `key[]=v` appends to an array; every other key overwrites an earlier value.
        if key.contains("[]") {
            None
        } else {
            Some(format!("field:{key}"))
        }
    };
    let (source, json, loc, key) = match kind {
        Kind::Text => (
            SourceKind::Inline(value.to_string()),
            false,
            None,
            scalar_key,
        ),
        Kind::File | Kind::Input => {
            let json = kind == Kind::Input;
            if value == "-" {
                (SourceKind::Stdin, json, None, scalar_key)
            } else {
                (
                    SourceKind::File(value.to_string()),
                    json,
                    location(rest, index, value),
                    scalar_key,
                )
            }
        }
        Kind::RawField => {
            let v = value.split_once('=').map(|(_, v)| v).unwrap_or(value);
            (
                SourceKind::Inline(v.to_string()),
                false,
                None,
                field_key(value),
            )
        }
        Kind::TypedField => {
            let v = value.split_once('=').map(|(_, v)| v).unwrap_or(value);
            match v.strip_prefix('@') {
                Some("-") => (SourceKind::Stdin, false, None, field_key(value)),
                Some(path) => (
                    SourceKind::File(path.to_string()),
                    false,
                    location(rest, index, path),
                    field_key(value),
                ),
                None => (
                    SourceKind::Inline(v.to_string()),
                    false,
                    None,
                    field_key(value),
                ),
            }
        }
        Kind::JsonStdin => (SourceKind::Stdin, true, None, scalar_key),
    };
    out.push(BodySource {
        flag,
        kind: source,
        json,
        location: loc,
        key,
    });
}

/// Keep only the sources gh will actually send: for a scalar flag (`--body`, `-b`,
/// `--body-file`) and for each `gh api` field key, the last occurrence wins, as in gh.
fn effective(all: Vec<BodySource>) -> Vec<BodySource> {
    let mut out: Vec<BodySource> = Vec::new();
    for s in all {
        if let Some(k) = &s.key {
            out.retain(|o| o.key.as_ref() != Some(k));
        }
        out.push(s);
    }
    out
}

/// Every body source gh will send on a WRITE command line. `rest` is the argument list after
/// the command words. `stdin_is_tty` matters only for `secret set`/`variable set`, which read the
/// value from stdin when no flag supplies it and stdin is not a terminal.
///
/// For an alias, an extension or an unknown command, every source is kept: gh's last-wins rule
/// does not apply to an alias's own arguments (`my-alias --body=A --body=B` may hand `A` to a
/// body field through `$1`), so none of them is dropped.
pub fn body_sources(c: &Classification, rest: &[String], stdin_is_tty: bool) -> Vec<BodySource> {
    let all = scan(c, rest, stdin_is_tty).sources;
    if crate::classify::is_unknown_command(c) {
        all
    } else {
        effective(all)
    }
}

/// Every file gh may read while running this command, including overridden occurrences (gh
/// reads some of those too). These are the files the wrapper snapshots.
pub fn file_sources(c: &Classification, rest: &[String]) -> Vec<BodySource> {
    scan(c, rest, true)
        .sources
        .into_iter()
        .filter(|s| matches!(s.kind, SourceKind::File(_)))
        .collect()
}

/// Replace inline body text in `rest` with `<N bytes>` markers, `gh api` header values with
/// `<redacted>`, and a `gh api` endpoint with its normalised form (no host, userinfo, query or
/// fragment), for audit records and messages. File paths are kept. For an alias, extension or
/// unknown command, and for a `gh api` call with a flag the parser does not know, every argument
/// that is not a flag name becomes `<arg>`: the unknown flag may take a value, and that value
/// may be a credential. On other commands, the value of an unknown `--flag=value` (other than a
/// boolean literal), the argument after an unknown bare flag, and a long run of unknown
/// shorthand letters (`-tTOKEN`) are dropped too.
pub fn redact(c: &Classification, rest: &[String]) -> Vec<String> {
    if crate::classify::is_unknown_command(c)
        || (c.family == "api" && crate::classify::parse_api(rest).unknown.is_some())
    {
        return redact_unknown(rest);
    }
    let mut out: Vec<String> = rest.to_vec();
    for (idx, replacement) in scan(c, rest, true).redactions {
        if let Some(slot) = out.get_mut(idx) {
            *slot = replacement;
        }
    }
    out
}

/// Flag names only: `--name=value` and `-xvalue` lose their values, everything else is `<arg>`.
pub fn redact_unknown(rest: &[String]) -> Vec<String> {
    rest.iter()
        .map(|t| {
            if t == "--" {
                t.clone()
            } else if let Some(long) = t.strip_prefix("--") {
                format!("--{}", long.split('=').next().unwrap_or(""))
            } else if t.len() > 1 && t.starts_with('-') {
                t.chars().take(2).collect()
            } else {
                "<arg>".to_string()
            }
        })
        .collect()
}

fn inline_marker(kind: Kind, value: &str) -> Option<String> {
    match kind {
        Kind::Text => Some(format!("<{} bytes>", value.len())),
        Kind::RawField | Kind::TypedField => {
            let (key, v) = value.split_once('=').unwrap_or(("", value));
            if kind == Kind::TypedField && v.starts_with('@') {
                return None;
            }
            Some(format!("{key}=<{} bytes>", v.len()))
        }
        Kind::File | Kind::Input | Kind::JsonStdin => None,
    }
}

type Redactions = Vec<(usize, String)>;

/// What one pass over the argument list found.
struct Scan {
    /// Every body source, in order, before last-wins resolution.
    sources: Vec<BodySource>,
    /// Audit replacements by argument index.
    redactions: Redactions,
    /// Every flag occurrence, in order: the canonical long name (or `-x` for a short flag that is
    /// not a body flag) and its value as a boolean. A flag that takes a value records
    /// `Some(true)` (present). A boolean records what gh would parse: `--web` and `-w` are
    /// `Some(true)`, `--web=false` and `-w=0` are `Some(false)`, and a value gh cannot parse as a
    /// boolean is `None` (gh exits with an error).
    flags: Vec<(String, Option<bool>)>,
    /// Positional arguments.
    positionals: usize,
}

/// Go's `strconv.ParseBool`, which pflag uses for `--flag=value` on a boolean flag.
pub(crate) fn parse_go_bool(v: &str) -> Option<bool> {
    match v {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// An argument of an alias, an extension or another write without a dedicated table, counted
/// as inline text: gh may hand any of them, flag-shaped or not, to a body flag.
fn generic_text(t: &str) -> BodySource {
    BodySource {
        flag: "<arg>".into(),
        kind: SourceKind::Inline(t.to_string()),
        json: false,
        location: None,
        key: None,
    }
}

fn scan(c: &Classification, rest: &[String], stdin_is_tty: bool) -> Scan {
    let table = table_for(c);
    let is_api = std::ptr::eq(table, &API);
    let generic = std::ptr::eq(table, &GENERIC);
    let mut out = Vec::new();
    let mut red: Redactions = Vec::new();
    let mut flags: Vec<(String, Option<bool>)> = Vec::new();
    let mut positionals = 0usize;
    // Set after a boolean or unrecognised flag written without `=value`: if gh does not know
    // that flag, the next argument may be its value, so the audit record drops it.
    let mut after_unknown = false;
    let mut positional =
        |out: &mut Vec<BodySource>, red: &mut Redactions, idx: usize, after_unknown: bool| {
            let t = rest[idx].as_str();
            if after_unknown && !is_api {
                red.push((idx, "<arg>".to_string()));
            }
            if is_api {
                // The first positional is the endpoint; gh api takes no other.
                if positionals == 0 {
                    red.push((idx, normalize_endpoint(t)));
                } else {
                    red.push((idx, "<arg>".to_string()));
                }
            } else if table.positional_files {
                if t == "-" {
                    out.push(BodySource {
                        flag: "<stdin>".into(),
                        kind: SourceKind::Stdin,
                        json: false,
                        location: None,
                        key: None,
                    });
                } else {
                    out.push(BodySource {
                        flag: "<file>".into(),
                        kind: SourceKind::File(t.to_string()),
                        json: false,
                        location: location(rest, idx, t),
                        key: None,
                    });
                }
            } else if table.positional_text {
                out.push(BodySource {
                    flag: "<arg>".into(),
                    kind: SourceKind::Inline(t.to_string()),
                    json: false,
                    location: None,
                    key: None,
                });
            }
            positionals += 1;
        };
    let mut i = 0;
    while i < rest.len() {
        let at = i;
        let t = rest[i].as_str();
        i += 1;
        let unknown_before = std::mem::take(&mut after_unknown);
        if t == "--" {
            for idx in i..rest.len() {
                positional(&mut out, &mut red, idx, false);
            }
            break;
        }
        if let Some(long) = t.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            if let Some((_, kind)) = table.long.iter().find(|(n, _)| *n == name) {
                if *kind == Kind::JsonStdin {
                    let value = inline.map_or(Some(true), parse_go_bool);
                    flags.push((name.to_string(), value));
                    if value == Some(false) {
                        // The last occurrence wins, as in pflag: a later `--json=false` means
                        // gh never reads stdin, so an earlier `--json`'s source is dropped.
                        out.retain(|s| s.key.as_deref() != Some(name));
                    } else {
                        push_source(&mut out, rest, name, *kind, "", at);
                    }
                    continue;
                }
                flags.push((name.to_string(), Some(true)));
                let (value, index) = match inline {
                    Some(v) => {
                        if let Some(m) = inline_marker(*kind, v) {
                            red.push((at, format!("--{name}={m}")));
                        }
                        (v.to_string(), at)
                    }
                    None => {
                        let v = rest.get(i).cloned().unwrap_or_default();
                        if let Some(m) = inline_marker(*kind, &v) {
                            red.push((i, m));
                        }
                        i += 1;
                        (v, i - 1)
                    }
                };
                push_source(&mut out, rest, name, *kind, &value, index);
            } else if table.value_long.contains(&name) {
                flags.push((name.to_string(), Some(true)));
                if generic {
                    out.push(generic_text(t));
                    if inline.is_none() {
                        if let Some(v) = rest.get(i) {
                            out.push(generic_text(v));
                        }
                    }
                }
                if is_api && name == "header" {
                    match inline {
                        Some(v) => red.push((at, format!("--header={}", redact_header(v)))),
                        None => {
                            if let Some(v) = rest.get(i) {
                                red.push((i, redact_header(v)));
                            }
                        }
                    }
                }
                if inline.is_none() {
                    i += 1;
                }
            } else {
                // A boolean, or a flag this table does not know.
                let value = inline.map_or(Some(true), parse_go_bool);
                flags.push((name.to_string(), value));
                match inline {
                    // Not a boolean literal: possibly a credential handed to an unknown flag.
                    Some(_) if value.is_none() => red.push((at, format!("--{name}=<arg>"))),
                    Some(_) => {}
                    None => after_unknown = true,
                }
                if generic {
                    out.push(generic_text(t));
                }
            }
            continue;
        }
        if t.len() > 1 && t.starts_with('-') {
            let chars: Vec<char> = t[1..].chars().collect();
            // Letters read as booleans (or unknown letters) before a value letter or the end.
            let mut bool_letters = 0usize;
            let mut consumed_body = false;
            for (j, ch) in chars.iter().enumerate() {
                if let Some((_, name)) = table.short.iter().find(|(s, _)| s == ch) {
                    consumed_body = true;
                    flags.push(((*name).to_string(), Some(true)));
                    let kind = table
                        .long
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, k)| *k)
                        .unwrap_or(Kind::Text);
                    let attached: String = chars[j + 1..].iter().collect();
                    let (value, index) = if attached.is_empty() {
                        let v = rest.get(i).cloned().unwrap_or_default();
                        if let Some(m) = inline_marker(kind, &v) {
                            red.push((i, m));
                        }
                        i += 1;
                        (v, i - 1)
                    } else {
                        let v = attached.strip_prefix('=').unwrap_or(&attached).to_string();
                        if let Some(m) = inline_marker(kind, &v) {
                            let prefix: String = chars[..=j].iter().collect();
                            red.push((at, format!("-{prefix}{m}")));
                        }
                        (v, at)
                    };
                    push_source(&mut out, rest, name, kind, &value, index);
                    break;
                }
                if table.value_short.contains(*ch) {
                    flags.push((format!("-{ch}"), Some(true)));
                    let attached: String = chars[j + 1..].iter().collect();
                    if generic && attached.is_empty() {
                        if let Some(v) = rest.get(i) {
                            out.push(generic_text(v));
                        }
                    }
                    if is_api && *ch == 'H' {
                        if attached.is_empty() {
                            if let Some(v) = rest.get(i) {
                                red.push((i, redact_header(v)));
                            }
                        } else {
                            let prefix: String = chars[..=j].iter().collect();
                            red.push((at, format!("-{prefix}{}", redact_header(&attached))));
                        }
                    }
                    if attached.is_empty() {
                        i += 1;
                    }
                    break;
                }
                // A boolean letter, or one this table does not know. pflag gives everything
                // after `=` to the letter before it (`-w=false`).
                bool_letters += 1;
                if chars.get(j + 1) == Some(&'=') {
                    let v: String = chars[j + 2..].iter().collect();
                    let value = parse_go_bool(&v);
                    flags.push((format!("-{ch}"), value));
                    if value.is_none() {
                        let prefix: String = chars[..=j].iter().collect();
                        red.push((at, format!("-{prefix}=<arg>")));
                    }
                    break;
                }
                flags.push((format!("-{ch}"), Some(true)));
                if j + 1 == chars.len() {
                    after_unknown = true;
                }
            }
            if bool_letters > 3 && !red.iter().any(|(idx, _)| *idx == at) {
                // A long run of letters no table knows is more likely a value glued to an
                // unknown shorthand (`-tTOKEN`) than a group of booleans.
                red.push((at, format!("-{}<arg>", chars[0])));
            }
            if generic && !consumed_body {
                out.push(generic_text(t));
            }
            continue;
        }
        positional(&mut out, &mut red, at, unknown_before);
    }
    if std::ptr::eq(table, &GIST_CREATE) && !out.iter().any(|s| s.flag != "--desc") {
        // `gh gist create` with no files reads the gist content from stdin.
        out.push(BodySource {
            flag: "<stdin>".into(),
            kind: SourceKind::Stdin,
            json: false,
            location: None,
            key: None,
        });
    }
    if table.stdin_when_no_body && out.is_empty() && !stdin_is_tty {
        out.push(BodySource {
            flag: "<stdin>".to_string(),
            kind: SourceKind::Stdin,
            json: false,
            location: None,
            key: None,
        });
    }
    Scan {
        sources: out,
        redactions: red,
        flags,
        positionals,
    }
}

/// A write whose body gh composes itself after this guard has run cannot be inspected: an
/// editor, an issue template, `--fill` from commit messages, release notes from a tag, or an
/// interactive prompt. Returns why, or `None` when every body the command sends is on the
/// command line, in a file, or on stdin. `stdin_is_tty` is whether gh could prompt.
///
/// Boolean flags are read for their effective value, as gh reads them: the last occurrence of
/// any spelling wins, and `--web=false` is off. A flag that would make the body uninspectable
/// counts when it is on or has a value gh cannot parse; a flag that exempts a form from the
/// prompt rule counts only when it is definitely on.
pub fn uninspectable(c: &Classification, rest: &[String], stdin_is_tty: bool) -> Option<String> {
    let s = scan(c, rest, stdin_is_tty);
    // None: absent. Some(v): the last occurrence of any of these spellings.
    let value_of = |spellings: &[&str]| -> Option<Option<bool>> {
        s.flags
            .iter()
            .rev()
            .find(|(n, _)| spellings.contains(&n.as_str()))
            .map(|(_, v)| *v)
    };
    let on = |spellings: &[&str]| matches!(value_of(spellings), Some(Some(true) | None));
    let set = |spellings: &[&str]| value_of(spellings) == Some(Some(true));
    let body_flag = set(&["body"]) || set(&["body-file"]);
    let web = set(&["web", "-w"]);
    let cmd = c.command.as_str();
    if c.family == "release" && on(&["notes-from-tag"]) {
        return Some(format!(
            "{cmd} --notes-from-tag takes the notes from a tag annotation or commit message that gh reads later"
        ));
    }
    match (c.family.as_str(), c.sub.as_deref()) {
        ("pr" | "issue", Some("create")) => {
            if on(&["template", "-T"]) {
                return Some(format!(
                    "{cmd} --template fills the body from a template file gh reads later"
                ));
            }
            if on(&["fill", "-f"]) || on(&["fill-first"]) || on(&["fill-verbose"]) {
                return Some(format!(
                    "{cmd} --fill composes the body from commit messages"
                ));
            }
            if on(&["editor", "-e"]) {
                return Some(format!("{cmd} --editor composes the body in an editor"));
            }
            // An interactive `issue create` takes its body only from the editor, which the
            // editor guard checks (see `crate::editor`). An interactive `pr create` offers a
            // body composed from the branch's commit messages and submits it without opening
            // the editor when the author just presses Enter, so that body is never seen.
            if c.family == "pr" && stdin_is_tty && !body_flag && !web && !set(&["recover"]) {
                return Some(format!(
                    "{cmd} without --body or --body-file prompts for the body and offers one composed from commit messages, which gh can submit without opening the editor"
                ));
            }
        }
        ("pr" | "issue", Some("comment")) => {
            if on(&["editor", "-e"]) {
                return Some(format!("{cmd} --editor composes the body in an editor"));
            }
            if stdin_is_tty && !body_flag && !web && !set(&["delete-last"]) {
                return Some(format!(
                    "{cmd} without --body or --body-file prompts for the body interactively"
                ));
            }
        }
        ("pr", Some("review")) => {
            let typed = set(&["approve", "-a"])
                || set(&["request-changes", "-r"])
                || set(&["comment", "-c"]);
            if stdin_is_tty && !body_flag && !typed {
                return Some(format!(
                    "{cmd} without --approve, --request-changes, --comment or a body prompts interactively"
                ));
            }
        }
        ("pr" | "issue", Some("edit")) => {
            let any_flag = s
                .flags
                .iter()
                .any(|(f, v)| !matches!(f.as_str(), "repo" | "-R") && *v == Some(true));
            if stdin_is_tty && !any_flag {
                return Some(format!(
                    "{cmd} with no flags prompts for the fields and the body interactively"
                ));
            }
        }
        // An interactive `pr merge` asks for the method and may open the editor for the merge
        // commit message; the editor guard checks that text (see `crate::editor`).
        ("release", Some("create")) => {
            let notes = set(&["notes"]) || set(&["notes-file"]) || set(&["generate-notes"]);
            if stdin_is_tty && !notes {
                return Some(format!(
                    "{cmd} without --notes, --notes-file or --generate-notes prompts for the notes interactively"
                ));
            }
        }
        ("gist", Some("edit"))
            if !(set(&["add"]) || set(&["remove", "-r"])) || s.positionals > 1 =>
        {
            return Some(format!(
                "{cmd} without --add or --remove opens an editor or replaces a file gh reads later"
            ));
        }
        _ => {}
    }
    None
}

/// Keep a header's name and drop its value (it may carry a credential).
fn redact_header(h: &str) -> String {
    match h.split_once(':') {
        Some((name, _)) => format!("{}: <redacted>", name.trim()),
        None => "<redacted>".to_string(),
    }
}

/// Characters of the standard and URL-safe base64 alphabets, plus padding.
fn is_b64(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-')
}

/// Length of the longest base64-looking content in `text`.
///
/// Four shapes count, whatever characters of the alphabet they use (a hex dump, a lower-case-only
/// encoding and a ruler all count, so nothing long slips through as a "word"):
///
/// - one unbroken run of base64-alphabet characters;
/// - a block of consecutive lines, each at least 20 characters and made only of base64-alphabet
///   characters, taken together. A list of bare commit SHAs, one per line, is such a block: 26
///   or more of them exceed the default 1,000-character limit. Write them with some prose on
///   each line, or set `GH_PACED_ALLOW_LARGE_BODY=1`;
/// - a block of consecutive base64-alphabet lines of any width that all have the same width,
///   except that the last may be shorter. This is the shape of an encoding wrapped at a fixed
///   column, however narrow, so wrapping at 16 or 8 columns does not hide one;
/// - a block of consecutive base64-alphabet lines of at least 4 characters, of any widths, each
///   holding an upper-case letter or made of one repeated character. Standard base64 has an
///   upper-case letter on nearly every line (it mixes the cases for most data, and the
///   encoding of zero bytes is all `A`), so an encoding wrapped at varying narrow widths (16,
///   then 12, then 16) forms this block, and so does a run of `/` (bytes 0xFF). A list of
///   lower-case identifiers or hex hashes, one per line, does not; a list of mixed-case or
///   upper-case names one per line does, once it passes the limit.
///
/// These are heuristics against an accidental upload, not a proof: an encoding broken up by
/// spaces or punctuation, or one wrapped at varying widths under 4 columns, or below 20
/// columns with no upper-case letter on some line (base32 and hex in lower case, URL-safe
/// base64 of unusual data), is not seen.
pub fn longest_base64_run(text: &[u8]) -> usize {
    let mut best = 0;
    let mut run = 0;
    for &b in text.iter().chain(std::iter::once(&b' ')) {
        if is_b64(b) {
            run += 1;
        } else {
            best = best.max(run);
            run = 0;
        }
    }
    let mut block = 0;
    // Block of any widths whose lines each hold an upper-case letter or repeat one character:
    // its total length.
    let mut cased = 0;
    // Equal-width block: its line width (0 when no block is open) and its total length.
    let mut width = 0;
    let mut even = 0;
    for line in text.split(|&b| b == b'\n').chain(std::iter::once(&b""[..])) {
        let trimmed = trim_ascii(line);
        let encoded = !trimmed.is_empty() && trimmed.iter().all(|&b| is_b64(b));
        if encoded && trimmed.len() >= 20 {
            block += trimmed.len();
        } else {
            best = best.max(block);
            block = 0;
        }
        if encoded
            && trimmed.len() >= 4
            && (trimmed.iter().any(u8::is_ascii_uppercase)
                || trimmed.iter().all(|&b| b == trimmed[0]))
        {
            cased += trimmed.len();
        } else {
            best = best.max(cased);
            cased = 0;
        }
        let len = trimmed.len();
        if !encoded {
            best = best.max(even);
            width = 0;
            even = 0;
        } else if width == len {
            even += len;
        } else if width > len {
            // A shorter line is the last line of a wrapped encoding: it ends the block. It may
            // also be the first line of the next one.
            best = best.max(even + len);
            width = len;
            even = len;
        } else {
            best = best.max(even);
            width = len;
            even = len;
        }
    }
    best.max(even)
}

fn trim_ascii(line: &[u8]) -> &[u8] {
    let start = line
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|p| p + 1)
        .unwrap_or(start);
    &line[start..end.max(start)]
}

fn json_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|v| json_strings(v, out)),
        serde_json::Value::Object(map) => map.values().for_each(|v| json_strings(v, out)),
        _ => {}
    }
}

/// Outcome of the content guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The write may proceed; `bytes` is the total body size inspected.
    Allow {
        /// Total bytes across all body sources.
        bytes: usize,
    },
    /// The write must be refused for the stated reason.
    Refuse(String),
}

/// Read at most `limit + 1` bytes of a file, returning the bytes and the file's full size.
fn read_capped(path: &str, limit: usize) -> Result<(Vec<u8>, u64), String> {
    let file =
        std::fs::File::open(path).map_err(|e| format!("cannot read body file {path}: {e}"))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut buf = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot read body file {path}: {e}"))?;
    let full = size.max(buf.len() as u64);
    Ok((buf, full))
}

/// Check the files gh composed in one editor session (see [`crate::editor`]) against the
/// write-body limits. `prior` is the body size the same gh call already gave on its command line
/// (the wrapper's [`Verdict::Allow`] total), so the editor text and the arguments share one
/// `max_body_bytes` allowance instead of getting one each.
pub fn check_composed_files(paths: &[String], cfg: &Config, prior: u64) -> Verdict {
    let sources: Vec<BodySource> = paths
        .iter()
        .map(|path| BodySource {
            flag: "the text saved in the editor".to_string(),
            kind: SourceKind::File(path.clone()),
            json: false,
            location: None,
            key: None,
        })
        .collect();
    evaluate_after(&sources, cfg, None, prior)
}

/// Inspect every body source. `stdin` holds the buffered stdin when a source reads it (the
/// wrapper buffers at most `max_body_bytes + 1` bytes; a longer stdin is refused before this).
pub fn evaluate(sources: &[BodySource], cfg: &Config, stdin: Option<&[u8]>) -> Verdict {
    evaluate_after(sources, cfg, stdin, 0)
}

/// [`evaluate`], with `prior` bytes of the size limit already used by an earlier check of the
/// same gh call.
fn evaluate_after(
    sources: &[BodySource],
    cfg: &Config,
    stdin: Option<&[u8]>,
    prior: u64,
) -> Verdict {
    let limit = cfg.max_body_bytes;
    let mut total: u64 = prior;
    // (flag, inspected bytes, parse as JSON, the source was longer than what was read)
    let mut texts: Vec<(String, Vec<u8>, bool, bool)> = Vec::new();
    for s in sources {
        let (bytes, size) = match &s.kind {
            SourceKind::Inline(v) => (v.as_bytes().to_vec(), v.len() as u64),
            SourceKind::File(path) => match read_capped(path, limit) {
                Ok(x) => x,
                Err(e) => return Verdict::Refuse(e),
            },
            SourceKind::Stdin => match stdin {
                Some(buf) => (buf.to_vec(), buf.len() as u64),
                None => {
                    return Verdict::Refuse(format!(
                        "{} reads stdin, which was not buffered for inspection",
                        s.flag
                    ))
                }
            },
        };
        total = total.saturating_add(size);
        let truncated = size > bytes.len() as u64;
        texts.push((s.flag.clone(), bytes, s.json, truncated));
    }
    // Report every reason, not just the first, so the author sees all that must change.
    let mut reasons = Vec::new();
    if total > limit as u64 {
        let earlier = if prior > 0 {
            format!(" (including {prior} bytes given on the command line)")
        } else {
            String::new()
        };
        reasons.push(format!(
            "write body is {total} bytes across {} source(s){earlier}, over the {limit}-byte limit",
            sources.len()
        ));
    }
    for (flag, bytes, json, truncated) in &texts {
        let mut run = longest_base64_run(bytes);
        if *json {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
                let mut strings = Vec::new();
                json_strings(&value, &mut strings);
                for s in strings {
                    run = run.max(longest_base64_run(s.as_bytes()));
                }
            }
        }
        if run > cfg.max_base64_run {
            let at_least = if *truncated { "at least " } else { "" };
            reasons.push(format!(
                "{flag} contains a base64-looking run of {at_least}{run} characters (limit {}); \
                 never store encoded data in GitHub text",
                cfg.max_base64_run
            ));
        }
    }
    if !reasons.is_empty() {
        return Verdict::Refuse(reasons.join("; "));
    }
    Verdict::Allow {
        bytes: usize::try_from(total).unwrap_or(usize::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::classify;

    fn argv(line: &[&str]) -> Vec<String> {
        line.iter().map(|s| s.to_string()).collect()
    }

    fn sources(line: &[&str]) -> Vec<BodySource> {
        let args = argv(line);
        let c = classify(&args, &Config::default());
        body_sources(&c, &args[c.rest_start..], true)
    }

    fn kinds(line: &[&str]) -> Vec<SourceKind> {
        sources(line).into_iter().map(|s| s.kind).collect()
    }

    fn src(flag: &str, kind: SourceKind, json: bool) -> BodySource {
        BodySource {
            flag: flag.into(),
            kind,
            json,
            location: None,
            key: None,
        }
    }

    fn inline(t: &str) -> SourceKind {
        SourceKind::Inline(t.into())
    }

    fn b64ish(n: usize) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut x: u32 = 12345;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                A[((x >> 16) % 64) as usize] as char
            })
            .collect()
    }

    /// `workflow run --json` reads its inputs from stdin; the last `--json` occurrence
    /// decides, so a later `--json=false` leaves no stdin source (gh never reads stdin).
    #[test]
    fn a_later_false_json_flag_drops_the_stdin_source() {
        let wf = |extra: &[&str]| {
            let mut line = vec!["workflow", "run", "build.yml"];
            line.extend_from_slice(extra);
            kinds(&line)
        };
        assert_eq!(wf(&["--json"]), vec![SourceKind::Stdin]);
        assert_eq!(wf(&["--json=false"]), Vec::<SourceKind>::new());
        assert_eq!(
            wf(&["--json", "--json=false", "-f", "foo=bar"]),
            vec![inline("bar")]
        );
        assert_eq!(wf(&["--json=false", "--json"]), vec![SourceKind::Stdin]);
        assert_eq!(
            wf(&["--json", "--json=0", "--json=true"]),
            vec![SourceKind::Stdin]
        );
    }

    #[test]
    fn finds_body_flags_per_command() {
        let s = sources(&["pr", "comment", "1", "-b", "hello"]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, inline("hello"));
        assert_eq!(s[0].flag, "--body");
        let s = sources(&["pr", "create", "-d", "-b", "body", "-t", "title"]);
        assert_eq!(s.len(), 2, "-d is --draft (bool) in pr create: {s:?}");
        let s = sources(&["issue", "create", "--body-file", "-"]);
        assert!(s[0].is_stdin());
        let s = sources(&["pr", "comment", "1", "-RFoo/Bar", "-bx"]);
        assert_eq!(
            s,
            vec![BodySource {
                flag: "--body".into(),
                kind: inline("x"),
                json: false,
                location: None,
                key: Some("body".into()),
            }]
        );
        assert_eq!(
            kinds(&["pr", "comment", "1", "--body=inline"]),
            vec![inline("inline")]
        );
        let s = sources(&["pr", "merge", "1", "--squash", "-t", "subj", "-b", "body"]);
        assert_eq!(s.len(), 2);
        let s = sources(&[
            "issue",
            "close",
            "1",
            "-c",
            "closing note",
            "-r",
            "completed",
        ]);
        assert_eq!(s.len(), 1);
        let s = sources(&["pr", "review", "1", "-c", "-b", "note"]);
        assert_eq!(s.len(), 1, "-c is --comment (bool) in pr review: {s:?}");
        let s = sources(&[
            "release",
            "create",
            "v1",
            "-n",
            "notes",
            "-F",
            "notes.md",
            "asset.bin",
        ]);
        assert_eq!(s.len(), 2);
        let s = sources(&["gist", "create", "-d", "desc", "a.txt", "-"]);
        assert_eq!(s.len(), 3);
        assert!(s[2].is_stdin());
        // `--recover` replays a saved JSON draft: it is a body source like `--input`.
        let s = sources(&["pr", "create", "--recover", "draft.json"]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, SourceKind::File("draft.json".into()));
        assert!(s[0].json);
        // `--template` names a template; its value is not a body source.
        assert!(sources(&["issue", "create", "-T", "bug.md"]).is_empty());
    }

    /// gh composes the gist from stdin when no files are named.
    #[test]
    fn gist_create_without_files_reads_stdin() {
        let s = sources(&["gist", "create", "-d", "desc"]);
        assert_eq!(s.len(), 2, "{s:?}");
        assert!(s[1].is_stdin());
        let s = sources(&["gist", "create"]);
        assert_eq!(s.len(), 1);
        assert!(s[0].is_stdin());
        let s = sources(&["gist", "create", "a.txt"]);
        assert_eq!(s, file_sources_of(&["gist", "create", "a.txt"]));
    }

    fn file_sources_of(line: &[&str]) -> Vec<BodySource> {
        let args = argv(line);
        let c = classify(&args, &Config::default());
        file_sources(&c, &args[c.rest_start..])
    }

    /// gh keeps the last value of a repeated scalar flag and of a repeated `gh api` field key,
    /// so only that value is sent and inspected; array fields (`key[]=`) all go.
    #[test]
    fn repeated_flags_last_value_wins() {
        let big = "word ".repeat(2000);
        assert_eq!(
            kinds(&["pr", "comment", "1", "--body", &big, "--body", "small"]),
            vec![inline("small")]
        );
        assert_eq!(
            kinds(&["pr", "comment", "1", "-b", &big, "--body=small"]),
            vec![inline("small")]
        );
        assert_eq!(
            kinds(&["pr", "comment", "1", "--body", "small", "-b", &big]),
            vec![inline(&big)]
        );
        // Different flags are different sources.
        assert_eq!(
            sources(&["pr", "comment", "1", "-b", "x", "--body-file", "f.md"]).len(),
            2
        );
        assert_eq!(
            kinds(&["api", "x", "-f", "body=A", "-f", "body=B"]),
            vec![inline("B")]
        );
        assert_eq!(
            kinds(&["api", "x", "-f", "body=A", "-F", "body=@f.md"]),
            vec![SourceKind::File("f.md".into())]
        );
        assert_eq!(
            kinds(&["api", "x", "-f", "a[]=1", "-f", "a[]=2"]),
            vec![inline("1"), inline("2")]
        );
        // Every file gh may read is still listed for the snapshot.
        assert_eq!(
            file_sources_of(&[
                "pr",
                "comment",
                "1",
                "--body-file",
                "a.md",
                "--body-file",
                "b.md"
            ])
            .len(),
            2
        );
    }

    /// File sources carry where their path is, so the wrapper can substitute a snapshot.
    #[test]
    fn file_sources_record_their_argument_position() {
        let loc = |line: &[&str]| -> Vec<(usize, String)> {
            file_sources_of(line)
                .into_iter()
                .map(|s| {
                    let l = s.location.expect("located");
                    (l.index, l.prefix)
                })
                .collect()
        };
        assert_eq!(
            loc(&["pr", "comment", "1", "--body-file", "a.md"]),
            vec![(2, String::new())]
        );
        assert_eq!(
            loc(&["pr", "comment", "1", "--body-file=a.md"]),
            vec![(1, "--body-file=".to_string())]
        );
        assert_eq!(
            loc(&["pr", "comment", "1", "-Fa.md"]),
            vec![(1, "-F".to_string())]
        );
        assert_eq!(
            loc(&["api", "x", "-F", "body=@a.md"]),
            vec![(2, "body=@".to_string())]
        );
        assert_eq!(
            loc(&["api", "x", "-Fbody=@a.md", "--input", "r.json"]),
            vec![(1, "-Fbody=@".to_string()), (3, String::new())]
        );
        assert_eq!(
            loc(&["gist", "create", "a.txt", "b.txt"]),
            vec![(0, String::new()), (1, String::new())]
        );
    }

    #[test]
    fn finds_api_body_sources() {
        let s = sources(&[
            "api",
            "repos/o/r/issues/1/comments",
            "-f",
            "body=hi",
            "-F",
            "n=@x.md",
            "-F",
            "m=@-",
            "-F",
            "k=3",
        ]);
        assert_eq!(s.len(), 4);
        assert_eq!(s[0].kind, inline("hi"));
        assert_eq!(s[1].kind, SourceKind::File("x.md".into()));
        assert!(s[2].is_stdin());
        assert_eq!(s[3].kind, inline("3"));
        let s = sources(&["api", "--method", "POST", "x", "--input", "req.json"]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].flag, "--input");
        assert_eq!(s[0].kind, SourceKind::File("req.json".into()));
        assert!(s[0].json);
        let s = sources(&["api", "-H", "-f", "x", "-fbody=y"]);
        assert_eq!(s.len(), 1, "-f after -H is the header value: {s:?}");
    }

    /// An alias or extension may turn any argument into body text, so each one counts.
    #[test]
    fn alias_arguments_count_as_body_text() {
        let big = b64ish(1200);
        let s = sources(&["my-alias", "first", &big]);
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!(s[1].kind, inline(&big));
        assert!(matches!(
            evaluate(&s, &Config::default(), None),
            Verdict::Refuse(m) if m.contains("base64")
        ));
    }

    #[test]
    fn secret_set_reads_stdin_only_when_not_a_terminal() {
        let args = argv(&["secret", "set", "NAME"]);
        let c = classify(&args, &Config::default());
        assert!(body_sources(&c, &args[2..], true).is_empty());
        assert!(body_sources(&c, &args[2..], false)[0].is_stdin());
    }

    fn refused(line: &[&str], tty: bool) -> Option<String> {
        let args = argv(line);
        let c = classify(&args, &Config::default());
        uninspectable(&c, &args[c.rest_start..], tty)
    }

    /// Bodies gh composes after the guard has run (editor, template, --fill, interactive
    /// prompt) cannot be inspected, so those forms are refused.
    #[test]
    fn uninspectable_forms_are_refused() {
        let always: &[&[&str]] = &[
            &["pr", "create", "-t", "x", "-b", "y", "--template", "t.md"],
            &["issue", "create", "-T", "bug.md"],
            &["pr", "create", "--fill"],
            &["pr", "create", "-f"],
            &["pr", "create", "--fill-first"],
            &["pr", "create", "--fill-verbose"],
            &["issue", "create", "-t", "x", "-e"],
            &["pr", "create", "--editor"],
            &["pr", "comment", "1", "-e"],
            &["issue", "comment", "1", "--editor"],
            &["gist", "edit", "abc"],
            &["gist", "edit", "abc", "file.txt"],
            &["gist", "edit", "abc", "src.txt", "--add", "name.txt"],
        ];
        for line in always {
            assert!(refused(line, false).is_some(), "not refused: {line:?}");
            assert!(refused(line, true).is_some(), "not refused (tty): {line:?}");
        }
        let prompts: &[&[&str]] = &[
            &["pr", "create", "-t", "title"],
            &["pr", "create"],
            &["pr", "comment", "1"],
            &["issue", "comment", "1", "--edit-last"],
            &["pr", "review", "1"],
            &["pr", "edit", "1"],
            &["issue", "edit", "1", "-R", "o/r"],
            &["release", "create", "v1"],
            &["release", "create", "v1", "-t", "title"],
        ];
        for line in prompts {
            assert!(refused(line, true).is_some(), "not refused (tty): {line:?}");
            assert!(
                refused(line, false).is_none(),
                "refused without a tty: {line:?}"
            );
        }
        // These prompt on a terminal too, but the only text they compose comes from the
        // editor, which gh opens through the editor guard (`crate::editor`; tested end to end in
        // tests/cli.rs), so they run.
        let editor_guarded: &[&[&str]] = &[
            &["issue", "create"],
            &["issue", "create", "-t", "title"],
            &["pr", "merge", "1"],
            &["pr", "merge", "1", "--auto", "-d"],
        ];
        for line in editor_guarded {
            assert!(refused(line, true).is_none(), "refused (tty): {line:?}");
            assert!(refused(line, false).is_none(), "refused: {line:?}");
        }
        let fine: &[&[&str]] = &[
            &["pr", "create", "-t", "x", "-b", "y"],
            &["pr", "create", "-t", "x", "--body-file", "b.md"],
            &["pr", "create", "--web"],
            &["pr", "create", "--recover", "d.json"],
            &["issue", "create", "-t", "x", "-F", "b.md"],
            &["pr", "comment", "1", "-b", "x"],
            &["pr", "comment", "1", "--edit-last", "-b", "x"],
            &["pr", "comment", "1", "--delete-last"],
            &["pr", "review", "1", "-a"],
            &["pr", "review", "1", "-c", "-b", "x"],
            &["pr", "edit", "1", "--add-label", "x"],
            &["pr", "merge", "1", "--squash"],
            &["pr", "merge", "1", "-r"],
            &["pr", "merge", "1", "--disable-auto"],
            &["release", "create", "v1", "--generate-notes"],
            &["release", "create", "v1", "-n", "notes"],
            &["release", "create", "v1", "-F", "n.md"],
            &["gist", "edit", "abc", "--add", "new.txt"],
            &["gist", "edit", "abc", "-a", "new.txt"],
            &["gist", "edit", "abc", "--remove", "old.txt"],
            &["gist", "edit", "abc", "-r", "old.txt"],
            &["issue", "close", "1"],
            &["label", "create", "bug"],
        ];
        for line in fine {
            assert!(refused(line, true).is_none(), "refused: {line:?}");
            assert!(refused(line, false).is_none(), "refused: {line:?}");
        }
    }

    /// Boolean flags count for their effective value: the last occurrence wins and `=false`
    /// turns a flag off, so `--web=false` earns no exemption and `--editor=false` no refusal.
    #[test]
    fn uninspectable_reads_boolean_values() {
        // Off: no exemption from the prompt rule on a terminal.
        for line in [
            &["issue", "comment", "1", "--web=false"][..],
            &["pr", "comment", "1", "-w=0"],
            &["pr", "comment", "1", "--web", "--web=false"],
            &["pr", "create", "-t", "x", "--web=False"],
            &["pr", "comment", "1", "--delete-last=f"],
            &["pr", "review", "1", "--approve=false"],
            &["pr", "review", "1", "-a=0"],
            // (`pr merge` has no prompt rule any more: the editor guard checks its commit
            // message. These keep the `--flag=false` and `-x=F` spellings covered.)
            &["pr", "review", "1", "--comment=false"],
            &["pr", "review", "1", "-c=F"],
            &["pr", "edit", "1", "--web=false"],
            &["release", "create", "v1", "--generate-notes=false"],
            // A value gh cannot parse never earns an exemption.
            &["pr", "comment", "1", "--web=yes"],
        ] {
            assert!(refused(line, true).is_some(), "not refused (tty): {line:?}");
        }
        // Off: no refusal for a form that would otherwise compose the body later.
        for line in [
            &["pr", "create", "-t", "x", "-b", "y", "--editor=false"][..],
            &["pr", "create", "-t", "x", "-b", "y", "-e=0"],
            &["pr", "create", "-t", "x", "-b", "y", "--fill=false"],
            &["pr", "create", "-t", "x", "-b", "y", "-f=false"],
            &[
                "pr",
                "create",
                "-t",
                "x",
                "-b",
                "y",
                "--fill",
                "--fill=false",
            ],
            &["pr", "create", "-t", "x", "-b", "y", "--fill-first=0"],
            &["pr", "comment", "1", "-b", "x", "--editor=false"],
            &[
                "release",
                "create",
                "v1",
                "-n",
                "x",
                "--notes-from-tag=false",
            ],
            &["pr", "comment", "1", "--web=false", "--web"],
        ] {
            assert!(refused(line, true).is_none(), "refused (tty): {line:?}");
            assert!(refused(line, false).is_none(), "refused: {line:?}");
        }
        // On, or unparseable: refused.
        for line in [
            &[
                "pr",
                "create",
                "-t",
                "x",
                "-b",
                "y",
                "--fill=false",
                "--fill",
            ][..],
            &["pr", "create", "-t", "x", "-b", "y", "--fill=maybe"],
            &["pr", "create", "-t", "x", "-b", "y", "--editor=1"],
            &["pr", "create", "-t", "x", "-b", "y", "-e=T"],
        ] {
            assert!(refused(line, false).is_some(), "not refused: {line:?}");
        }
    }

    /// gh fills `--notes-from-tag` notes from a tag annotation or commit message after the guard
    /// has run, so the form is refused (`GH_PACED_ALLOW_LARGE_BODY=1` skips the whole guard).
    #[test]
    fn notes_from_tag_is_refused() {
        for line in [
            &["release", "create", "v1", "--notes-from-tag"][..],
            &["release", "create", "v1", "-n", "x", "--notes-from-tag"],
            &[
                "release",
                "create",
                "v1",
                "--notes-from-tag=true",
                "--generate-notes",
            ],
            &["release", "edit", "v1", "--notes-from-tag"],
        ] {
            let why = refused(line, false).unwrap_or_default();
            assert!(why.contains("--notes-from-tag"), "{line:?}: {why:?}");
            assert!(refused(line, true).is_some(), "{line:?}");
        }
    }

    /// An alias can hand any argument, flag-shaped or not, to a body flag in its expansion, so
    /// every argument of an alias or extension is inspected as text.
    #[test]
    fn alias_flag_shaped_arguments_are_inspected() {
        let big = "word ".repeat(2000);
        let payload = format!("--payload={big}");
        for line in [
            &["my-alias", payload.as_str()][..],
            &["my-alias", "--repo", big.as_str()],
            &["my-alias", "-R", big.as_str()],
            &["my-alias", "--", big.as_str()],
        ] {
            let s = sources(line);
            assert!(
                matches!(
                    evaluate(&s, &Config::default(), None),
                    Verdict::Refuse(ref m) if m.contains("over the 8192-byte limit")
                ),
                "{line:?}: {s:?}"
            );
        }
        let enc = format!("--x={}", b64ish(1200));
        let s = sources(&["my-alias", &enc]);
        assert!(matches!(
            evaluate(&s, &Config::default(), None),
            Verdict::Refuse(m) if m.contains("base64")
        ));
        // gh's last-wins rule does not apply to an alias's arguments: every occurrence counts.
        let body = format!("--body={big}");
        for line in [
            &["my-alias", body.as_str(), "--body=small"][..],
            &["my-alias", "-b", big.as_str(), "-b", "small"],
        ] {
            let s = sources(line);
            assert!(
                matches!(
                    evaluate(&s, &Config::default(), None),
                    Verdict::Refuse(ref m) if m.contains("over the 8192-byte limit")
                ),
                "{line:?}: {s:?}"
            );
        }
        // A known command keeps gh's last-wins rule.
        let s = sources(&["issue", "comment", "1", body.as_str(), "--body=small"]);
        assert!(matches!(
            evaluate(&s, &Config::default(), None),
            Verdict::Allow { .. }
        ));
        // A small alias call is still allowed.
        let s = sources(&["my-alias", "--flag=small", "-x", "words"]);
        assert!(matches!(
            evaluate(&s, &Config::default(), None),
            Verdict::Allow { .. }
        ));
    }

    #[test]
    fn opaque_commands_beneath_known_families_are_inspected_and_redacted() {
        let big = "word ".repeat(2000);
        let body = format!("--body={big}");
        // `extension exec` and an alias beneath a group (`issue publish`) hand every argument to
        // code gh-paced cannot see, so gh's last-wins rule does not apply to them either.
        for line in [
            &["extension", "exec", "my-ext", body.as_str(), "--body=small"][..],
            &["issue", "publish", body.as_str(), "--body=small"],
            &["issue", "publish", "-b", big.as_str(), "-b", "small"],
            &["issue", "publish", big.as_str()],
            &["config", "publish", big.as_str()],
        ] {
            let s = sources(line);
            assert!(
                matches!(
                    evaluate(&s, &Config::default(), None),
                    Verdict::Refuse(ref m) if m.contains("over the 8192-byte limit")
                ),
                "{line:?}: {s:?}"
            );
        }
        // Positional text is redacted in the audit, not copied into it.
        assert_eq!(
            redacted(&["extension", "exec", "my-ext", "BODY_CANARY", "--token=T"]),
            argv(&["extension", "exec", "<arg>", "<arg>", "--token"])
        );
        assert_eq!(
            redacted(&["issue", "publish", "BODY_CANARY", "-b", "SECRET"]),
            argv(&["issue", "publish", "<arg>", "-b", "<arg>"])
        );
    }

    fn redacted(line: &[&str]) -> Vec<String> {
        let args = argv(line);
        let c = classify(&args, &Config::default());
        let mut out = args[..c.rest_start].to_vec();
        out.extend(redact(&c, &args[c.rest_start..]));
        out
    }

    #[test]
    fn redaction_drops_bodies_headers_and_endpoint_queries() {
        assert_eq!(
            redacted(&["pr", "comment", "1", "-b", "secret words", "-R", "o/r"]),
            argv(&["pr", "comment", "1", "-b", "<12 bytes>", "-R", "o/r"])
        );
        assert_eq!(
            redacted(&["api", "x", "-H", "Authorization: token SECRET"]),
            argv(&["api", "x", "-H", "Authorization: <redacted>"])
        );
        let r = redacted(&[
            "api",
            "https://user:pw@api.github.com/repos/o/r/issues?access_token=github_pat_CANARY#frag",
        ]);
        assert_eq!(r, argv(&["api", "repos/o/r/issues"]));
        let r = redacted(&["api", "repos/o/r/issues?access_token=github_pat_CANARY"]);
        assert_eq!(r, argv(&["api", "repos/o/r/issues"]));
    }

    /// A flag the parser does not know may take a value, and the value may be a credential.
    #[test]
    fn unknown_flag_values_are_redacted() {
        assert_eq!(
            redacted(&["api", "repos/o/r", "--token=CANARY"]),
            argv(&["api", "<arg>", "--token"])
        );
        assert_eq!(
            redacted(&["api", "--token", "CANARY", "repos/o/r"]),
            argv(&["api", "--token", "<arg>", "<arg>"])
        );
        assert_eq!(
            redacted(&["api", "-ZCANARY", "repos/o/r"]),
            argv(&["api", "-Z", "<arg>"])
        );
        for line in [
            &["pr", "comment", "1", "-b", "x", "--tokn=CANARY"][..],
            &["pr", "comment", "1", "--tokn", "CANARY"],
            &["pr", "comment", "1", "-tCANARY"],
            &["pr", "comment", "1", "-t=CANARY"],
            &["pr", "view", "1", "--tokn=CANARY"],
        ] {
            let r = redacted(line);
            assert!(!r.iter().any(|a| a.contains("CANARY")), "{line:?} -> {r:?}");
        }
        // Boolean literals and known value flags are kept.
        assert_eq!(
            redacted(&["pr", "comment", "1", "--web=false", "-R", "o/r", "-b", "x"]),
            argv(&[
                "pr",
                "comment",
                "1",
                "--web=false",
                "-R",
                "o/r",
                "-b",
                "<1 bytes>"
            ])
        );
    }

    /// An alias or extension keeps only its name and its flag names.
    #[test]
    fn unknown_commands_keep_only_flag_names() {
        let args = argv(&[
            "my-alias",
            "SECRET_WORDS",
            "-b",
            "BODY_CANARY",
            "--token=TOKEN_CANARY",
            "-xVALUE_CANARY",
            "--",
            "tail",
        ]);
        let c = classify(&args, &Config::default());
        assert_eq!(c.command, "my-alias");
        assert_eq!(c.rest_start, 1);
        let r = redacted(&args.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(
            r,
            argv(&["my-alias", "<arg>", "-b", "<arg>", "--token", "-x", "--", "<arg>"])
        );
        // An unrecognised flag before any command word.
        let r = redacted(&["--weird", "SECRET"]);
        assert_eq!(r, argv(&["--weird", "<arg>"]));
    }

    #[test]
    fn base64_detection() {
        // Ordinary words are short runs ("prose", "words").
        assert_eq!(longest_base64_run(b"plain prose with words."), 5);
        let blob = b64ish(1500);
        assert_eq!(longest_base64_run(blob.as_bytes()), 1500);
        // Wrapped at 76 columns, inside a fenced block, the lines still count together.
        let wrapped: String = blob
            .as_bytes()
            .chunks(76)
            .map(|c| format!("  {}\n", String::from_utf8_lossy(c)))
            .collect();
        let text = format!("Here is data:\n```\n{wrapped}```\n");
        assert_eq!(longest_base64_run(text.as_bytes()), 1500);
        // Canonical base64 with no digits ("abc" repeated, zero bytes, and `i\xa6\x9a` repeated,
        // which encodes to lower-case letters only) still counts, both as one run and wrapped.
        for unit in ["YWJj", "AAAA", "aaaa"] {
            let enc = unit.repeat(300);
            assert_eq!(longest_base64_run(enc.as_bytes()), 1200, "{unit}");
            let wrapped: String = enc
                .as_bytes()
                .chunks(76)
                .map(|c| format!("{}\n", String::from_utf8_lossy(c)))
                .collect();
            assert_eq!(
                longest_base64_run(wrapped.as_bytes()),
                1200,
                "{unit} wrapped"
            );
        }
        // One unbroken run counts whatever its alphabet: a long hex dump and a long ruler too.
        let hex = "0123456789abcdef".repeat(100);
        assert_eq!(longest_base64_run(hex.as_bytes()), 1600);
        assert_eq!(longest_base64_run("-".repeat(2000).as_bytes()), 2000);
        // A list of bare lower-case commit SHAs, one per line, is a block too: hex is an
        // encoding, and the detector cannot tell digests from data. 40 lines of 40 = 1600.
        let shas: String = (0..40)
            .map(|i| format!("{:040x}\n", 0x1234_5678_9abc_u64 * (i + 1)))
            .collect();
        assert_eq!(longest_base64_run(shas.as_bytes()), 1600);
        // The same SHAs with a subject on each line are prose, not a block.
        let log: String = (0..40)
            .map(|i| format!("{:040x} fix the thing\n", 0x1234_5678_9abc_u64 * (i + 1)))
            .collect();
        assert_eq!(longest_base64_run(log.as_bytes()), 40);
        // Short lines of one width are an encoding wrapped at a narrow column: they form a block
        // (this was 10 before the equal-width rule; the guard now refuses it).
        let short: String = (0..200).map(|_| "abcdefghij\n").collect();
        assert_eq!(longest_base64_run(short.as_bytes()), 2000);
        // Canonical base64 wrapped at 16 and at 8 columns, with a shorter last line, counts in full.
        for cols in [16, 8] {
            let enc = format!("{}YWI=", "YWJj".repeat(300));
            let wrapped: String = enc
                .as_bytes()
                .chunks(cols)
                .map(|c| format!("{}\r\n", String::from_utf8_lossy(c)))
                .collect();
            assert_eq!(
                longest_base64_run(wrapped.as_bytes()),
                1204,
                "{cols} columns"
            );
        }
        // A list of identifiers of differing widths, one per line, is not an encoding.
        let names: String = (0..200)
            .map(|i| format!("test_case_{}\n", "x".repeat(i % 7)))
            .collect();
        assert!(longest_base64_run(names.as_bytes()) < 100, "{names}");
        // The list sizes the user guide quotes: 7-character short SHAs and 4-digit numbers, one
        // per line, cross the default 1,000 at 143 and at 251 lines.
        let shas = |n: u64| -> String {
            (0..n)
                .map(|i| format!("{:07x}\n", 0xa1_b2c3 + i * 977))
                .collect()
        };
        assert_eq!(longest_base64_run(shas(142).as_bytes()), 994);
        assert_eq!(longest_base64_run(shas(143).as_bytes()), 1001);
        let numbers =
            |n: u64| -> String { (0..n).map(|i| format!("{}\n", 1000 + i * 7)).collect() };
        assert_eq!(longest_base64_run(numbers(250).as_bytes()), 1000);
        assert_eq!(longest_base64_run(numbers(251).as_bytes()), 1004);
    }

    /// Canonical base64 wrapped at alternating narrow widths (16, 12, 16, ...) is one encoded
    /// block, not a list of short identifiers, and the guard refuses it.
    #[test]
    fn varying_narrow_wrapping_is_detected() {
        let enc = "YWJj".repeat(300);
        for eol in ["\n", "\r\n"] {
            let mut wrapped = String::new();
            let mut rest = enc.as_str();
            let mut wide = true;
            while !rest.is_empty() {
                let (line, tail) = rest.split_at(rest.len().min(if wide { 16 } else { 12 }));
                wrapped.push_str(line);
                wrapped.push_str(eol);
                rest = tail;
                wide = !wide;
            }
            assert_eq!(longest_base64_run(wrapped.as_bytes()), 1200, "{eol:?}");
            let body = vec![src("-b", inline(&wrapped), false)];
            assert!(
                matches!(evaluate(&body, &Config::default(), None), Verdict::Refuse(m) if m.contains("base64")),
                "{eol:?}"
            );
        }
        // Single-case standard base64 wrapped the same way: 900 zero bytes are 1,200 `A`s, 900
        // 0xFF bytes 1,200 `/`s, and mostly-zero data is upper case with a few digits.
        let zeros = "A".repeat(1200);
        let ones = "/".repeat(1200);
        let sparse = "AAAAAAAAAAAAAAE0".repeat(75);
        for enc in [&zeros, &ones, &sparse] {
            let mut wrapped = String::new();
            let mut rest = enc.as_str();
            let mut wide = true;
            while !rest.is_empty() {
                let (line, tail) = rest.split_at(rest.len().min(if wide { 16 } else { 12 }));
                wrapped.push_str(line);
                wrapped.push('\n');
                rest = tail;
                wide = !wide;
            }
            assert_eq!(longest_base64_run(wrapped.as_bytes()), 1200, "{enc}");
        }
        // Lower-case words and identifiers of varying widths, one per line, are not a block.
        let words: String = (0..300)
            .map(|i| format!("{}\n", ["alpha", "beta_2", "gamma-ray", "delta"][i % 4]))
            .collect();
        assert!(longest_base64_run(words.as_bytes()) < 20, "{words}");
        // Mixed-case prose keeps its spaces, so it is not a block either.
        let prose: String = (0..300).map(|_| "Fix The Thing\n").collect();
        assert!(longest_base64_run(prose.as_bytes()) < 20);
    }

    #[test]
    fn editor_text_shares_the_allowance_with_the_arguments() {
        let dir = std::env::temp_dir().join(format!("gh-paced-composed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("body.md");
        std::fs::write(&file, "word ".repeat(1638)).unwrap();
        let paths = vec![file.display().to_string()];
        let cfg = Config::default();
        // 8,190 bytes pass on their own, but not after a 200-byte title.
        assert_eq!(
            check_composed_files(&paths, &cfg, 0),
            Verdict::Allow { bytes: 8190 }
        );
        match check_composed_files(&paths, &cfg, 200) {
            Verdict::Refuse(r) => assert!(
                r.contains("write body is 8390 bytes across 1 source(s) (including 200 bytes given on the command line)"),
                "{r}"
            ),
            v => panic!("{v:?}"),
        }
        // Two files from one session are counted together.
        let two = vec![paths[0].clone(), paths[0].clone()];
        assert!(matches!(
            check_composed_files(&two, &cfg, 0),
            Verdict::Refuse(_)
        ));
        // A nonsense prior saturates rather than overflowing, and refuses.
        assert!(matches!(
            check_composed_files(&paths, &cfg, u64::MAX),
            Verdict::Refuse(_)
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn evaluate_size_and_base64() {
        let cfg = Config::default();
        let ok = vec![src("-b", inline(&"x".repeat(4000)), false)];
        // 4000 identical letters are one 4000-character run.
        assert!(matches!(evaluate(&ok, &cfg, None), Verdict::Refuse(m) if m.contains("base64")));
        let ok = vec![src("-b", inline(&"word ".repeat(800)), false)];
        assert_eq!(evaluate(&ok, &cfg, None), Verdict::Allow { bytes: 4000 });
        let big = vec![src("-b", inline(&"word ".repeat(2000)), false)];
        assert!(matches!(evaluate(&big, &cfg, None),
            Verdict::Refuse(m) if m.contains("10000 bytes across 1 source(s), over the 8192-byte limit")
                && !m.contains("base64")));
        let enc = vec![src("-b", inline(&b64ish(1200)), false)];
        assert!(matches!(evaluate(&enc, &cfg, None), Verdict::Refuse(m) if m.contains("base64")));
        // Two sources that are each small but together too large.
        let two = vec![
            src("-t", inline(&"a ".repeat(2100)), false),
            src("-b", inline(&"b ".repeat(2100)), false),
        ];
        assert!(matches!(evaluate(&two, &cfg, None),
            Verdict::Refuse(m) if m.contains("8400 bytes across 2 source(s)")));
        // JSON request body: base64 split by escaped newlines is found after decoding.
        let lines: Vec<String> = b64ish(1200)
            .as_bytes()
            .chunks(60)
            .map(|c| String::from_utf8_lossy(c).to_string())
            .collect();
        let json = serde_json::json!({"body": lines.join("\n")}).to_string();
        let stdin_src = vec![src("--input", SourceKind::Stdin, true)];
        assert!(matches!(
            evaluate(&stdin_src, &cfg, Some(json.as_bytes())),
            Verdict::Refuse(_)
        ));
        // Unreadable file is refused, not waved through.
        let missing = vec![src("-F", SourceKind::File("/nonexistent/x".into()), false)];
        assert!(matches!(evaluate(&missing, &cfg, None), Verdict::Refuse(_)));
        // Stdin source without a buffer is refused.
        assert!(matches!(
            evaluate(&stdin_src, &cfg, None),
            Verdict::Refuse(_)
        ));
    }

    /// An `--input` request file shaped like a chunked-archive upload: far over the size limit and
    /// one long base64 line inside the JSON `body`. Both reasons are reported, and the run length
    /// is marked as a lower bound because only the first `limit + 1` bytes are read.
    #[test]
    fn oversized_encoded_input_file_reports_both_reasons() {
        let dir = std::env::temp_dir().join(format!("gh-paced-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("part.request.json");
        let body = format!("part 01/21\n\n```\n{}\n```", b64ish(60_000));
        std::fs::write(&path, serde_json::json!({ "body": body }).to_string()).unwrap();
        let file_src = vec![src(
            "--input",
            SourceKind::File(path.display().to_string()),
            true,
        )];
        let verdict = evaluate(&file_src, &Config::default(), None);
        let Verdict::Refuse(m) = verdict else {
            panic!("allowed: {verdict:?}")
        };
        assert!(m.contains("over the 8192-byte limit"), "{m}");
        assert!(
            m.contains("--input contains a base64-looking run of at least"),
            "{m}"
        );
        // A small prose request file passes.
        std::fs::write(
            &path,
            serde_json::json!({ "body": "word ".repeat(800) }).to_string(),
        )
        .unwrap();
        assert!(matches!(
            evaluate(&file_src, &Config::default(), None),
            Verdict::Allow { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
