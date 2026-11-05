//! Classify one `gh` command line into a request class and a token cost.
//!
//! The classifier never touches the network. It is deliberately conservative: anything it does
//! not recognise as LOCAL, READ, SEARCH or GIT_CREDENTIAL is WRITE, so an alias, an extension, a
//! new `gh` subcommand, or an unparseable command line is paced as the most expensive class.

use crate::config::Config;
use serde::{Deserialize, Serialize};

/// Request class. Each non-LOCAL class has its own token bucket and hourly window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// No GitHub network traffic (help, version, completion, local config).
    Local,
    /// Reads from the core REST or GraphQL API.
    Read,
    /// Search API reads, which GitHub limits separately and more tightly.
    Search,
    /// Anything that may create or change content, and anything unrecognised.
    Write,
    /// `gh auth git-credential get`: one per git network operation.
    GitCredential,
}

impl Class {
    /// Lower-case name used in state, config and audit records.
    pub fn name(self) -> &'static str {
        match self {
            Class::Local => "local",
            Class::Read => "read",
            Class::Search => "search",
            Class::Write => "write",
            Class::GitCredential => "git_credential",
        }
    }

    /// Every class that has a budget.
    pub const PACED: [Class; 4] = [
        Class::Read,
        Class::Search,
        Class::Write,
        Class::GitCredential,
    ];
}

/// What one `gh api` invocation will do, as far as the command line says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiCall {
    /// HTTP method, upper case (explicit `-X`, or the implied GET/POST).
    pub method: String,
    /// Endpoint with any scheme, host and leading slash removed.
    pub endpoint: String,
    /// `--paginate` or `--slurp` was given.
    pub paginate: bool,
    /// `-i/--include` was given: gh prints the response status line and headers on stdout,
    /// which the wrapper then scans for pushback.
    pub include: bool,
}

/// The classifier's verdict for one command line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Classification {
    /// Request class.
    pub class: Class,
    /// Tokens charged against the class bucket and hourly window.
    pub cost: u32,
    /// Top-level command (`pr`, `api`, ...), or empty for a bare `gh`.
    pub family: String,
    /// Subcommand (`comment`, `view`, ...), when the family has subcommands.
    pub sub: Option<String>,
    /// Short command description for messages and audit (`pr comment`, `api GET repos/o/r`).
    pub command: String,
    /// Why this class was chosen.
    pub reason: String,
    /// Loud warnings to print before the call (expensive pagination, watch loops).
    pub warnings: Vec<String>,
    /// When set, the call must be refused before it reaches GitHub (exit 64).
    pub refusal: Option<String>,
    /// Details of a `gh api` call.
    pub api: Option<ApiCall>,
    /// Index in the argument vector where the arguments after the command words begin.
    pub rest_start: usize,
    /// Wall-clock bound for a polling command (watch loops). The wrapper kills the child when it
    /// runs this long, so a loop can never poll beyond the tokens it was charged.
    pub deadline_secs: Option<f64>,
    /// The command line is not a complete built-in command: an alias, an extension, a
    /// passthrough command (`extension exec`, `copilot`), an unknown subcommand beneath a group
    /// (`issue publish`), or an unrecognised flag before the command was complete. Every later
    /// argument is opaque: inspected as possible body text and redacted in the audit.
    pub opaque: bool,
}

/// READ tokens charged for `gh status`: the default READ burst, the most one call may cost.
pub const STATUS_COST: u32 = 10;

const LOCAL_FAMILIES: &[&str] = &[
    "completion",
    "config",
    "alias",
    "help",
    "version",
    "accessibility",
    "a11y",
    "actions",
    "environment",
    "exit-codes",
    "formatting",
    "mintty",
    "reference",
];

/// Families whose second word is an argument, not a subcommand.
const NO_SUB_FAMILIES: &[&str] = &[
    "api",
    "browse",
    "status",
    "completion",
    "version",
    "credits",
    "help",
    "accessibility",
    "a11y",
    "actions",
    "environment",
    "exit-codes",
    "formatting",
    "mintty",
    "reference",
];

fn normalize_sub(sub: &str) -> &str {
    match sub {
        "ls" => "list",
        "co" => "checkout",
        "new" => "create",
        "rm" => "delete",
        other => other,
    }
}

/// READ subcommands per family (after alias normalisation).
fn read_subs(family: &str) -> &'static [&'static str] {
    match family {
        "pr" => &["view", "list", "status", "checks", "diff", "checkout"],
        "issue" => &["view", "list", "status"],
        "run" => &["view", "list", "download", "watch"],
        "workflow" => &["view", "list"],
        "repo" => &[
            "view",
            "list",
            "clone",
            "set-default",
            "gitignore",
            "license",
        ],
        "release" => &["view", "list", "download", "verify", "verify-asset"],
        "label" => &["list"],
        "gist" => &["view", "list", "clone"],
        "secret" => &["list"],
        "variable" => &["list", "get"],
        "cache" => &["list"],
        "ssh-key" | "gpg-key" => &["list"],
        "org" => &["list"],
        "project" => &["view", "list", "field-list", "item-list"],
        "ruleset" => &["view", "list", "check"],
        "attestation" => &["verify", "download", "trusted-root"],
        "codespace" => &["view", "list"],
        "extension" => &["list", "browse"],
        "agent-task" => &["view", "list"],
        _ => &[],
    }
}

/// Families that are gh's own commands (not an alias or an extension). The empty family is a
/// bare `gh --help`.
fn is_known_family(family: &str) -> bool {
    family.is_empty()
        || LOCAL_FAMILIES.contains(&family)
        || NO_SUB_FAMILIES.contains(&family)
        || !read_subs(family).is_empty()
        || matches!(family, "auth" | "search")
}

fn is_flag(token: &str) -> bool {
    token.len() > 1 && token.starts_with('-')
}

/// How a node of gh's command tree treats the word that follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    /// A command group such as `pr` or `repo autolink`: not runnable. A word that is not one of
    /// its subcommands names a user alias, because gh adds an alias such as `issue publish` as a
    /// child of the group it extends (or it names a gh command this table does not know).
    Group,
    /// A runnable command: a word after it is an argument.
    Leaf,
    /// A runnable command that also has subcommands (`codespace ports`): a known subcommand
    /// descends, any other word is an argument of the command itself.
    RunnableGroup,
    /// A help topic such as `environment`: not runnable and without subcommands, so a word after
    /// it can only name an alias.
    Topic,
    /// A command that turns flag parsing off and hands every later argument to another program
    /// (`extension exec`, `copilot`). Its arguments are opaque, like an alias's.
    Passthrough,
}

/// One command in gh's command tree.
struct Node {
    name: &'static str,
    aliases: &'static [&'static str],
    kind: NodeKind,
    children: &'static [Node],
}

const fn node(
    name: &'static str,
    aliases: &'static [&'static str],
    kind: NodeKind,
    children: &'static [Node],
) -> Node {
    Node {
        name,
        aliases,
        kind,
        children,
    }
}

const fn leaf(name: &'static str) -> Node {
    node(name, &[], NodeKind::Leaf, &[])
}

/// A runnable command with cobra aliases, such as `list` (`ls`).
const fn leaf_a(name: &'static str, aliases: &'static [&'static str]) -> Node {
    node(name, aliases, NodeKind::Leaf, &[])
}

const fn group(
    name: &'static str,
    aliases: &'static [&'static str],
    children: &'static [Node],
) -> Node {
    node(name, aliases, NodeKind::Group, children)
}

const fn topic(name: &'static str, aliases: &'static [&'static str]) -> Node {
    node(name, aliases, NodeKind::Topic, &[])
}

const LS: &[&str] = &["ls"];
const NEW: &[&str] = &["new"];

/// gh 2.97.0's built-in command tree, with cobra's own aliases (`ls`, `co`, `new`, `cs`, ...).
/// Taken from `gh help reference` and gh's hidden commands. A command that is missing here is
/// treated as opaque (WRITE, every argument inspected and redacted), so an omission costs a
/// token, never protection; but a group listed as a runnable command would let an alias beneath
/// it pass as that command's argument, so every command with subcommands is a group.
const ROOT: &[Node] = &[
    group(
        "agent-task",
        &["agent-tasks", "agent", "agents"],
        &[leaf("create"), leaf("list"), leaf("view")],
    ),
    group(
        "alias",
        &[],
        &[
            leaf("delete"),
            leaf("import"),
            leaf_a("list", LS),
            leaf("set"),
        ],
    ),
    leaf("api"),
    group(
        "attestation",
        &["at"],
        &[leaf("download"), leaf("trusted-root"), leaf("verify")],
    ),
    group(
        "auth",
        &[],
        &[
            leaf("login"),
            leaf("logout"),
            leaf("refresh"),
            leaf("setup-git"),
            leaf("status"),
            leaf("switch"),
            leaf("token"),
            leaf("git-credential"),
        ],
    ),
    leaf("browse"),
    group("cache", &[], &[leaf("delete"), leaf_a("list", LS)]),
    group(
        "codespace",
        &["cs"],
        &[
            leaf("code"),
            leaf("cp"),
            leaf("create"),
            leaf("delete"),
            leaf("edit"),
            leaf("jupyter"),
            leaf_a("list", LS),
            leaf("logs"),
            node(
                "ports",
                &[],
                NodeKind::RunnableGroup,
                &[leaf("forward"), leaf("visibility")],
            ),
            leaf("rebuild"),
            leaf("ssh"),
            leaf("stop"),
            leaf("view"),
        ],
    ),
    leaf("completion"),
    group(
        "config",
        &[],
        &[
            leaf("clear-cache"),
            leaf("get"),
            leaf_a("list", LS),
            leaf("set"),
        ],
    ),
    node("copilot", &[], NodeKind::Passthrough, &[]),
    group(
        "discussion",
        &[],
        &[
            leaf("comment"),
            leaf("create"),
            leaf("edit"),
            leaf_a("list", LS),
            leaf("view"),
        ],
    ),
    group(
        "extension",
        &["extensions", "ext"],
        &[
            leaf("browse"),
            leaf("create"),
            node("exec", &[], NodeKind::Passthrough, &[]),
            leaf("install"),
            leaf_a("list", LS),
            leaf_a("remove", &["uninstall"]),
            leaf("search"),
            leaf("upgrade"),
        ],
    ),
    group(
        "gist",
        &[],
        &[
            leaf("clone"),
            leaf_a("create", NEW),
            leaf("delete"),
            leaf("edit"),
            leaf_a("list", LS),
            leaf("rename"),
            leaf("view"),
        ],
    ),
    group(
        "gpg-key",
        &[],
        &[leaf("add"), leaf("delete"), leaf_a("list", LS)],
    ),
    group(
        "issue",
        &[],
        &[
            leaf("close"),
            leaf("comment"),
            leaf_a("create", NEW),
            leaf("delete"),
            leaf("develop"),
            leaf("edit"),
            leaf_a("list", LS),
            leaf("lock"),
            leaf("pin"),
            leaf("reopen"),
            leaf("status"),
            leaf("transfer"),
            leaf("unlock"),
            leaf("unpin"),
            leaf("view"),
        ],
    ),
    group(
        "label",
        &[],
        &[
            leaf("clone"),
            leaf("create"),
            leaf("delete"),
            leaf("edit"),
            leaf_a("list", LS),
        ],
    ),
    leaf("licenses"),
    group("org", &[], &[leaf_a("list", LS)]),
    group(
        "pr",
        &[],
        &[
            leaf_a("checkout", &["co"]),
            leaf("checks"),
            leaf("close"),
            leaf("comment"),
            leaf_a("create", NEW),
            leaf("diff"),
            leaf("edit"),
            leaf_a("list", LS),
            leaf("lock"),
            leaf("merge"),
            leaf("ready"),
            leaf("reopen"),
            leaf("revert"),
            leaf("review"),
            leaf("status"),
            leaf("unlock"),
            leaf("update-branch"),
            leaf("view"),
        ],
    ),
    group("preview", &[], &[leaf("prompter")]),
    group(
        "project",
        &[],
        &[
            leaf("close"),
            leaf("copy"),
            leaf("create"),
            leaf("delete"),
            leaf("edit"),
            leaf("field-create"),
            leaf("field-delete"),
            leaf("field-list"),
            leaf("item-add"),
            leaf("item-archive"),
            leaf("item-create"),
            leaf("item-delete"),
            leaf("item-edit"),
            leaf("item-list"),
            leaf("link"),
            leaf_a("list", LS),
            leaf("mark-template"),
            leaf("unlink"),
            leaf("view"),
        ],
    ),
    group(
        "release",
        &[],
        &[
            leaf_a("create", NEW),
            leaf("delete"),
            leaf("delete-asset"),
            leaf("download"),
            leaf("edit"),
            leaf_a("list", LS),
            leaf("upload"),
            leaf("verify"),
            leaf("verify-asset"),
            leaf("view"),
        ],
    ),
    group(
        "repo",
        &[],
        &[
            leaf("archive"),
            group(
                "autolink",
                &[],
                &[
                    leaf_a("create", NEW),
                    leaf("delete"),
                    leaf_a("list", LS),
                    leaf("view"),
                ],
            ),
            leaf("clone"),
            leaf_a("create", NEW),
            leaf("delete"),
            group(
                "deploy-key",
                &[],
                &[leaf("add"), leaf("delete"), leaf_a("list", LS)],
            ),
            leaf("edit"),
            leaf("fork"),
            group("gitignore", &[], &[leaf_a("list", LS), leaf("view")]),
            group("license", &[], &[leaf_a("list", LS), leaf("view")]),
            leaf_a("list", LS),
            leaf("read-dir"),
            leaf("read-file"),
            leaf("rename"),
            leaf("set-default"),
            leaf("sync"),
            leaf("unarchive"),
            leaf("view"),
        ],
    ),
    group(
        "ruleset",
        &["rs"],
        &[leaf("check"), leaf_a("list", LS), leaf("view")],
    ),
    group(
        "run",
        &[],
        &[
            leaf("cancel"),
            leaf("delete"),
            leaf("download"),
            leaf_a("list", LS),
            leaf("rerun"),
            leaf("view"),
            leaf("watch"),
        ],
    ),
    group(
        "search",
        &[],
        &[
            leaf("code"),
            leaf("commits"),
            leaf("issues"),
            leaf("prs"),
            leaf("repos"),
        ],
    ),
    group(
        "secret",
        &[],
        &[
            leaf_a("delete", &["remove"]),
            leaf_a("list", LS),
            leaf("set"),
        ],
    ),
    group(
        "skill",
        &["skills"],
        &[
            leaf_a("install", &["add"]),
            leaf_a("list", LS),
            leaf_a("preview", &["show"]),
            leaf("publish"),
            leaf("search"),
            leaf("update"),
        ],
    ),
    group(
        "ssh-key",
        &[],
        &[leaf("add"), leaf("delete"), leaf_a("list", LS)],
    ),
    leaf("status"),
    group(
        "variable",
        &[],
        &[
            leaf_a("delete", &["remove"]),
            leaf("get"),
            leaf_a("list", LS),
            leaf("set"),
        ],
    ),
    group(
        "workflow",
        &[],
        &[
            leaf("disable"),
            leaf("enable"),
            leaf_a("list", LS),
            leaf("run"),
            leaf("view"),
        ],
    ),
    // Hidden runnable commands.
    leaf("credits"),
    leaf("version"),
    leaf("help"),
    // Help topics.
    topic("accessibility", &["a11y"]),
    topic("actions", &[]),
    topic("environment", &[]),
    topic("exit-codes", &[]),
    topic("formatting", &[]),
    topic("mintty", &[]),
    topic("reference", &[]),
];

/// The command words at the front of a `gh` argument vector, found by walking [`ROOT`].
struct Lead {
    /// Canonical command words (`pr checkout` for `pr co`), plus the operation word of
    /// `auth git-credential`. An alias or extension name is not included (see `opaque_word`).
    words: Vec<String>,
    /// Index in the argument vector where the arguments after the command begin. For an alias
    /// or extension, that is just after its name.
    rest: usize,
    /// The first flag the walk did not recognise, when it stopped there.
    unknown_flag: Option<String>,
    /// The command line is not a complete built-in command whose flags gh-paced models: an
    /// alias or extension name, a passthrough command, or an unrecognised flag before the
    /// command was complete. Its arguments are opaque.
    opaque: bool,
    /// The word that named an alias, extension or unknown command.
    opaque_word: Option<String>,
    /// The walk ended on a command group without choosing a subcommand (gh prints help).
    bare_group: bool,
}

/// Walk gh's command tree the way cobra's `Find` does, skipping the flags gh-paced knows can
/// appear before the command words (`-R/--repo VALUE`, `--help`, `-h`, `--version`).
fn lead(args: &[String]) -> Lead {
    let mut words: Vec<String> = Vec::new();
    let mut children: &[Node] = ROOT;
    // The root behaves as a group: an unknown first word is an alias or an extension.
    let mut kind = NodeKind::Group;
    let mut i = 0;
    let mut unknown_flag = None;
    let mut opaque = false;
    let mut opaque_word = None;
    while i < args.len() {
        let t = args[i].as_str();
        if is_flag(t) {
            if t == "-R" || t == "--repo" {
                i += 2;
                continue;
            }
            if t.starts_with("--repo=") || (t.starts_with("-R") && t.len() > 2) {
                i += 1;
                continue;
            }
            if matches!(t, "--help" | "-h" | "--version") {
                i += 1;
                continue;
            }
            if kind != NodeKind::RunnableGroup {
                // Before the command is complete, an unknown flag may take the next word as its
                // value, so what follows cannot be read as command words.
                unknown_flag = Some(t.to_string());
                opaque = true;
            }
            break;
        }
        match children
            .iter()
            .find(|n| n.name == t || n.aliases.contains(&t))
        {
            Some(n) => {
                words.push(n.name.to_string());
                i += 1;
                kind = n.kind;
                children = n.children;
                match n.kind {
                    NodeKind::Passthrough => {
                        opaque = true;
                        break;
                    }
                    NodeKind::Leaf => {
                        if words == ["auth", "git-credential"] {
                            if let Some(op) = args.get(i).filter(|w| !is_flag(w)) {
                                words.push(op.clone());
                                i += 1;
                            }
                        }
                        break;
                    }
                    NodeKind::Group | NodeKind::RunnableGroup | NodeKind::Topic => {}
                }
            }
            None if kind == NodeKind::RunnableGroup => break,
            None => {
                opaque = true;
                opaque_word = Some(t.to_string());
                i += 1;
                break;
            }
        }
    }
    let rest = i.min(args.len());
    Lead {
        bare_group: !opaque && kind == NodeKind::Group && !words.is_empty() && rest == args.len(),
        words,
        rest,
        unknown_flag,
        opaque,
        opaque_word,
    }
}

/// The built-in command named `word` beneath the built-in command path `path` (canonical names,
/// empty for the root), as cobra's `findNext` matches it (name or cobra alias, exact case).
/// Returns its canonical name and whether gh can add a user alias beneath it: only commands
/// that are not runnable (a group or a help topic) take aliases. `None`: `path` is not a
/// built-in path, or `word` is not one of its built-in children.
pub fn builtin_child(path: &[&str], word: &str) -> Option<(&'static str, bool)> {
    let mut children: &[Node] = ROOT;
    for p in path {
        children = children.iter().find(|n| n.name == *p)?.children;
    }
    children
        .iter()
        .find(|n| n.name == word || n.aliases.contains(&word))
        .map(|n| (n.name, matches!(n.kind, NodeKind::Group | NodeKind::Topic)))
}

/// `--help` as the first flag, or `-h` as the last token, after nothing but bare words.
fn is_help_request(args: &[String]) -> bool {
    match args.iter().position(|t| is_flag(t)) {
        Some(idx) => args[idx] == "--help" || (args[idx] == "-h" && idx + 1 == args.len()),
        None => false,
    }
}

/// Every value of a flag given as `-X v`, `-Xv`, `-X=v`, `--long v` or `--long=v`, in order,
/// including a shorthand inside a group such as `-dL1000`.
///
/// gh (pflag) lets the last occurrence of a scalar flag win. Callers that turn a value into a
/// cost or a limit take the most conservative of all occurrences instead, so the answer does not
/// depend on which occurrence gh honours.
///
/// The result is a superset of what pflag would read. In a group, pflag consumes boolean
/// shorthands one letter at a time until it meets a shorthand that takes a value, which then
/// takes the rest of the token (or the next argument). gh-paced does not know every command's
/// boolean letters, so it treats every letter as possibly boolean and keeps scanning, stopping
/// only at `=` (pflag gives the rest of the token to the letter before it) and at `R`, the global
/// `--repo` shorthand, which always takes a value. Nor does it know which other flags take a
/// value, so a token it reads as a value is still examined as a flag in its own right: in
/// `--label -dL --limit 1000`, gh reads `-dL` as the label, and gh-paced records both `--limit`
/// (from `-dL`) and `1000`. A wrong guess can only add a candidate, never hide one; callers
/// choose the most conservative candidate.
///
/// For the same reason a `--` does not stop the scan. gh reads `--` as the end of the flags
/// only when no flag before it takes it as a value: in `pr list --label -- --limit 1000`, `--`
/// is the label and gh fetches 1000 items. Values after a real `--` are positional arguments,
/// so reading them can only add a candidate.
fn flag_values<'a>(rest: &'a [String], short: Option<char>, long: &str) -> Vec<&'a str> {
    let long_eq = format!("{long}=");
    let mut out = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let t = rest[i].as_str();
        if t == long {
            if let Some(v) = rest.get(i + 1) {
                out.push(v.as_str());
            }
            i += 1;
            continue;
        }
        if let Some(v) = t.strip_prefix(long_eq.as_str()) {
            out.push(v);
            i += 1;
            continue;
        }
        if let (Some(c), Some(group)) = (short, t.strip_prefix('-')) {
            if !group.is_empty() && !group.starts_with('-') {
                match short_group_value(group, c) {
                    GroupValue::Attached(v) => out.push(v),
                    GroupValue::Next => {
                        if let Some(v) = rest.get(i + 1) {
                            out.push(v.as_str());
                        }
                    }
                    GroupValue::Absent => {}
                }
            }
        }
        i += 1;
    }
    out
}

/// What a shorthand group (the token without its leading `-`) says about one value shorthand.
#[derive(Debug, PartialEq, Eq)]
enum GroupValue<'a> {
    /// The letter is not in the group, or pflag would not read it as a flag.
    Absent,
    /// The letter takes the rest of the token (`-L100`, `-dL100`, `-L=100`).
    Attached(&'a str),
    /// The letter ends the token and takes the next argument (`-L 100`, `-dL 100`).
    Next,
}

fn short_group_value(group: &str, target: char) -> GroupValue<'_> {
    for (idx, ch) in group.char_indices() {
        if ch == target {
            let after = &group[idx + ch.len_utf8()..];
            let after = after.strip_prefix('=').unwrap_or(after);
            return if after.is_empty() {
                GroupValue::Next
            } else {
                GroupValue::Attached(after)
            };
        }
        if ch == '=' || ch == 'R' {
            return GroupValue::Absent;
        }
    }
    GroupValue::Absent
}

/// Whether a boolean long flag (`--watch`, `--watch=false`) may be on, as gh would read it: the
/// last occurrence wins. A value gh cannot parse as a boolean counts as on (gh exits with an
/// error, so charging for it costs nothing real).
///
/// Each `--` is either the end of the flags or the value of the flag before it, and gh-paced
/// does not know which flags take a value. So the flag counts as on when it is on just before
/// any `--` (that `--` ended the flags) or at the end (every `--` was a value).
fn bool_flag_set(rest: &[String], long: &str) -> bool {
    let long_eq = format!("{long}=");
    let mut on = false;
    for t in rest {
        if t == "--" {
            if on {
                return true;
            }
            continue;
        }
        if t == long {
            on = true;
        } else if let Some(v) = t.strip_prefix(long_eq.as_str()) {
            on = crate::guard::parse_go_bool(v) != Some(false);
        }
    }
    on
}

/// Parse an integer flag value the way pflag does (Go's `strconv.ParseInt(s, 0, 64)`): an
/// optional sign, then `0x`/`0X` (hexadecimal), `0o`/`0O` or a bare leading `0` (octal), `0b`/`0B`
/// (binary) or plain decimal digits, with `_` allowed between digits. Slightly more permissive
/// than Go about where `_` may appear, which can only add a candidate value.
fn parse_go_int(value: &str) -> Option<i128> {
    let v = value.trim();
    let (negative, digits) = match v.as_bytes().first() {
        Some(b'-') => (true, &v[1..]),
        Some(b'+') => (false, &v[1..]),
        _ => (false, v),
    };
    let lower = digits.to_ascii_lowercase();
    let (radix, body) = if let Some(b) = lower.strip_prefix("0x") {
        (16, b)
    } else if let Some(b) = lower.strip_prefix("0o") {
        (8, b)
    } else if let Some(b) = lower.strip_prefix("0b") {
        (2, b)
    } else if lower.len() > 1 && lower.starts_with('0') {
        (8, &lower[1..])
    } else {
        (10, lower.as_str())
    };
    let body: String = body.chars().filter(|&c| c != '_').collect();
    if body.is_empty() || body.len() > 70 {
        return None;
    }
    let n = i128::from_str_radix(&body, radix).ok()?;
    Some(if negative { -n } else { n })
}

/// Parse a watch `--interval` value the way gh must read it to sleep for that many seconds.
///
/// gh declares `--interval` as an integer flag (pflag `IntVar`, which parses with Go's
/// `strconv.ParseInt(s, 0, 64)`), and sleeps `interval` seconds between polls; zero or a negative
/// value means no sleep at all. Only a plain positive decimal integer is accepted here. Anything
/// else is refused rather than guessed at: `0x1e` and `036` are valid integers to gh but in base
/// 16 and base 8, and `30s` is rejected by gh anyway.
fn parse_interval(value: &str) -> Option<f64> {
    let v = value.trim();
    let plain = !v.is_empty()
        && v.len() <= 9
        && !v.starts_with('0')
        && v.bytes().all(|b| b.is_ascii_digit());
    if !plain {
        return None;
    }
    v.parse::<u32>().ok().map(f64::from)
}

fn local(family: &str, sub: Option<String>, command: String, reason: &str) -> Classification {
    Classification {
        class: Class::Local,
        cost: 0,
        family: family.to_string(),
        sub,
        command,
        reason: reason.to_string(),
        warnings: Vec::new(),
        refusal: None,
        api: None,
        rest_start: 0,
        deadline_secs: None,
        opaque: false,
    }
}

/// Classify a `gh` argument vector (without the `gh` program name).
pub fn classify(args: &[String], cfg: &Config) -> Classification {
    let lead = lead(args);
    let mut c = classify_inner(args, &lead, cfg);
    // For an alias or extension, everything after its name is its own argument list, which may
    // carry body text, so none of it is treated as a command word.
    c.rest_start = if c.class == Class::Local {
        args.len()
    } else {
        lead.rest
    };
    c
}

/// The verdict for a command line that is not a complete built-in command (see
/// [`Lead::opaque`]): WRITE, with every later argument opaque.
fn opaque_command(lead: &Lead) -> Classification {
    let path = lead.words.join(" ");
    let mut c = match (&lead.opaque_word, lead.words.first()) {
        (Some(word), None) => fail_safe(
            word,
            None,
            word.clone(),
            "unknown command (alias, extension, or new gh command)",
        ),
        (Some(word), Some(family)) => {
            let mut sub: Vec<&str> = lead.words[1..].iter().map(String::as_str).collect();
            sub.push(word);
            fail_safe(
                family,
                Some(sub.join(" ")),
                format!("{path} {word}"),
                &format!(
                    "`{word}` is not a subcommand of `gh {path}`, so it names an alias (gh adds an alias such as `issue publish` beneath the group it extends) or a gh command this wrapper does not know"
                ),
            )
        }
        (None, _) if lead.unknown_flag.is_some() => fail_safe(
            lead.words.first().map_or("", String::as_str),
            lead.words.get(1).cloned(),
            if path.is_empty() {
                "gh".into()
            } else {
                path.clone()
            },
            &format!(
                "unrecognised flag {} before the {}",
                lead.unknown_flag.as_deref().map_or("?".into(), flag_name),
                if lead.words.is_empty() {
                    "command"
                } else {
                    "subcommand"
                }
            ),
        ),
        (None, _) => fail_safe(
            lead.words.first().map_or("", String::as_str),
            lead.words.get(1).cloned(),
            path.clone(),
            "hands every later argument to another program, which may write",
        ),
    };
    c.opaque = true;
    c
}

fn classify_inner(args: &[String], lead: &Lead, cfg: &Config) -> Classification {
    if args.is_empty() {
        return local("", None, "gh".into(), "bare gh prints help");
    }
    if args.len() == 1 && (args[0] == "--version" || args[0] == "version") {
        return local(
            "version",
            None,
            "version".into(),
            "prints the local version",
        );
    }
    // Checked before help and before the local families: `gh issue publish --help`,
    // `gh config publish` and `gh --help publish` run a user alias named `publish`, which receives
    // `--help` as an argument and may do anything with it (an alias ending in `-b` turns it into a
    // comment body). `gh extension exec` and `gh copilot` hand `--help` to another program.
    if lead.opaque {
        return opaque_command(lead);
    }
    let words = &lead.words;
    if is_help_request(args) {
        // The whole command path is one of gh's own commands, which print help for `--help`.
        return local(
            words.first().map_or("", String::as_str),
            None,
            format!("{} --help", words.join(" ")).trim().to_string(),
            "help output is local",
        );
    }
    let rest = &args[lead.rest..];
    let Some(family) = words.first().map(String::as_str) else {
        return fail_safe("", None, "gh".into(), "no command");
    };
    if lead.bare_group {
        return local(
            family,
            None,
            words.join(" "),
            "a command group without a subcommand prints help",
        );
    }
    if LOCAL_FAMILIES.contains(&family) {
        return local(
            family,
            words.get(1).cloned(),
            words.join(" "),
            "no GitHub traffic",
        );
    }
    if family == "api" {
        return classify_api(rest, cfg);
    }
    if family == "browse" {
        return paced(
            Class::Read,
            1,
            family,
            None,
            "browse".into(),
            "opens a URL; may look up the repository",
        );
    }
    if family == "credits" {
        return paced(
            Class::Read,
            1,
            family,
            None,
            "credits".into(),
            "reads the contributor list",
        );
    }
    if family == "status" {
        // Its searches are GraphQL `search` queries, which draw on the GraphQL points that READ
        // tracks (as `gh api graphql` and `pr list --search` do), not on the REST search limit;
        // its other requests are REST reads (notifications, events), more when there are many
        // notifications. Charged the whole default READ burst, the most one call may cost.
        return paced(
            Class::Read,
            STATUS_COST,
            family,
            None,
            "status".into(),
            "gh status makes several GraphQL and REST read requests (charged the 10-token read burst)",
        );
    }
    let Some(raw_sub) = words.get(1).map(String::as_str) else {
        return fail_safe(
            family,
            None,
            family.to_string(),
            "command that may contact GitHub",
        );
    };
    let sub = normalize_sub(raw_sub);
    let command = format!("{family} {sub}");
    let sub_owned = Some(sub.to_string());
    if family == "auth" {
        return match sub {
            "status" => paced(
                Class::Read,
                1,
                family,
                sub_owned,
                command,
                "validates the token against the API",
            ),
            "token" | "setup-git" | "switch" => {
                local(family, sub_owned, command, "local credential store only")
            }
            "git-credential" => match words.get(2).map(String::as_str) {
                Some("get") => paced(
                    Class::GitCredential,
                    1,
                    family,
                    sub_owned,
                    "auth git-credential get".into(),
                    "git asks for a credential before each network operation",
                ),
                Some(op @ ("store" | "erase")) => local(
                    family,
                    sub_owned,
                    format!("auth git-credential {op}"),
                    "gh ignores store and erase",
                ),
                _ => fail_safe(
                    family,
                    sub_owned,
                    command,
                    "unknown git-credential operation",
                ),
            },
            _ => fail_safe(
                family,
                sub_owned,
                command,
                "auth operation that may contact GitHub",
            ),
        };
    }
    if family == "search" {
        let mut c = paced(Class::Search, 1, family, sub_owned, command, "search API");
        apply_limit_cost(&mut c, rest);
        return c;
    }
    if family == "extension" && sub == "search" {
        let mut c = paced(
            Class::Search,
            1,
            family,
            sub_owned,
            command,
            "searches repositories",
        );
        apply_limit_cost(&mut c, rest);
        return c;
    }
    if family == "repo" && (sub == "autolink" || sub == "deploy-key") {
        let third = words.get(2).map(|w| normalize_sub(w)).unwrap_or("");
        let command = format!("{family} {sub} {third}").trim().to_string();
        if matches!(third, "list" | "view") {
            return paced(Class::Read, 1, family, sub_owned, command, "lists or views");
        }
        return fail_safe(family, sub_owned, command, "changes repository settings");
    }
    if read_subs(family).contains(&sub) {
        let mut c = paced(
            Class::Read,
            1,
            family,
            sub_owned,
            command,
            "read-only subcommand",
        );
        apply_limit_cost(&mut c, rest);
        apply_watch(&mut c, rest, cfg);
        return c;
    }
    // Every path that reaches here is one of gh's own commands (an alias or an extension was
    // handled by `opaque_command`).
    fail_safe(
        family,
        sub_owned,
        words.join(" "),
        "subcommand that may create or change content",
    )
}

/// True when the arguments of `c` must be treated as opaque: `c` is an alias, an extension, a
/// passthrough or unknown command ([`Classification::opaque`]), or one of gh's own commands in a
/// family whose flags this classifier does not model (`discussion`, `skill`, `codespace`, ...).
/// Every argument is then inspected as possible body text, gh's last-wins rule is not applied,
/// and the audit records flag names only, replacing every other argument with `<arg>`.
pub fn is_unknown_command(c: &Classification) -> bool {
    c.opaque
        || (c.class != Class::Local
            && c.family != "api"
            && (c.family.is_empty() || !is_known_family(&c.family)))
}

fn paced(
    class: Class,
    cost: u32,
    family: &str,
    sub: Option<String>,
    command: String,
    reason: &str,
) -> Classification {
    Classification {
        class,
        cost,
        family: family.to_string(),
        sub,
        command,
        reason: reason.to_string(),
        warnings: Vec::new(),
        refusal: None,
        api: None,
        rest_start: 0,
        deadline_secs: None,
        opaque: false,
    }
}

fn fail_safe(family: &str, sub: Option<String>, command: String, why: &str) -> Classification {
    paced(
        Class::Write,
        1,
        family,
        sub,
        command,
        &format!("{why}; classified WRITE (fail safe)"),
    )
}

/// `-L/--limit N` on a list or search: gh fetches up to 100 items per request.
fn apply_limit_cost(c: &mut Classification, rest: &[String]) {
    // The largest of all `--limit` values, whichever occurrence gh honours.
    // Values are read as pflag reads them (`--limit=0x3e8` is 1000). A value gh cannot parse, or
    // a negative one, makes gh exit with an error before any request.
    let largest = flag_values(rest, Some('L'), "--limit")
        .into_iter()
        .filter_map(parse_go_int)
        .filter(|&n| n > 0)
        .map(|n| u64::try_from(n).unwrap_or(u64::MAX))
        .max();
    if let Some(n) = largest {
        let pages = n.div_ceil(100).max(1);
        let pages = u32::try_from(pages).unwrap_or(u32::MAX);
        if pages > c.cost {
            c.warnings.push(format!(
                "--limit {n} may fetch {pages} pages; charging {pages} {} tokens",
                c.class.name()
            ));
            c.cost = pages;
        }
    }
}

/// Watch loops poll GitHub for as long as they run.
///
/// The loop is charged `watch_cost` tokens up front and given a deadline that the purchased
/// tokens are estimated to cover: `floor((cost - startup) / requests per poll) * interval`.
///
/// The per-poll figures are estimates with a margin, not a proof:
///
/// * `gh run watch` fetches the run, its workflow and its jobs on every poll (three requests),
///   plus one more page per 100 jobs and one annotations request the first time each job is
///   seen to fail. It is charged 4 per poll and 2 for startup (resolving the run).
/// * `gh pr checks --watch` fetches the checks once per poll (one GraphQL request per 100
///   checks). It is charged 2 per poll and 2 for startup (resolving the pull request).
///
/// A run with more than 100 jobs, or with several jobs that fail during the watch, can make more
/// requests than it paid for before the deadline stops it. The deadline still bounds the run's
/// wall time, and the interval floor bounds its request rate.
///
/// Because gh-paced cannot count those requests, a watch is refused unless
/// `GH_PACED_ALLOW_WATCH=1` accepts the estimate. Polling with repeated plain calls instead
/// (`gh pr checks`, `gh run view`) is paced call by call.
fn apply_watch(c: &mut Classification, rest: &[String], cfg: &Config) {
    let (is_watch, default_interval, startup, requests_per_poll) =
        match (c.family.as_str(), c.sub.as_deref()) {
            ("pr", Some("checks")) => (bool_flag_set(rest, "--watch"), 10.0, 2, 2),
            ("run", Some("watch")) => (true, 3.0, 2, 4),
            _ => (false, 0.0, 0, 1),
        };
    if !is_watch {
        return;
    }
    // Every `--interval` value must be one gh sleeps for. gh honours the last occurrence, so a
    // single non-positive or unreadable value anywhere is refused, and the smallest valid value
    // sets the pace.
    let values = flag_values(rest, Some('i'), "--interval");
    if let Some(bad) = values.iter().find(|v| parse_interval(v).is_none()) {
        c.refusal = Some(format!(
            "{} has --interval {bad:?}, which is not a positive whole number of seconds; gh would poll without sleeping or reject it. Pass `--interval {}`",
            c.command, cfg.min_watch_interval_secs
        ));
        return;
    }
    let interval = values
        .iter()
        .filter_map(|v| parse_interval(v))
        .reduce(f64::min)
        .unwrap_or(default_interval);
    c.cost = c.cost.max(cfg.watch_cost).max(startup + requests_per_poll);
    let polls = (f64::from(c.cost - startup) / f64::from(requests_per_poll))
        .floor()
        .max(1.0);
    let deadline = polls * interval;
    c.deadline_secs = Some(deadline);
    c.warnings.push(format!(
        "{} polls GitHub every {interval} s until it finishes; charging {} read tokens up front, estimated to cover startup plus {polls} polls of about {requests_per_poll} requests, so it is stopped after {deadline} s",
        c.command, c.cost
    ));
    if interval < cfg.min_watch_interval_secs && !cfg.allow_fast_watch {
        c.refusal = Some(format!(
            "{} would poll every {interval} s, faster than the {} s minimum; pass `--interval {}` (or set GH_PACED_ALLOW_FAST_WATCH=1)",
            c.command, cfg.min_watch_interval_secs, cfg.min_watch_interval_secs
        ));
    }
    if !cfg.allow_watch {
        let mut text = format!(
            "{} polls GitHub inside one gh call, and gh-paced cannot count the requests each poll makes (more than 100 jobs or checks, or failing jobs, add requests), so it cannot hold the call to what it charged. Poll with repeated plain calls instead, each of which is paced: run `gh pr checks <pr>` or `gh run view <run>` about once a minute in a loop. Or set GH_PACED_ALLOW_WATCH=1 to accept the estimated charge of {} read tokens and the {deadline} s deadline",
            c.command, c.cost
        );
        if let Some(other) = c.refusal.take() {
            text.push_str("; also, ");
            text.push_str(&other);
        }
        c.refusal = Some(text);
    }
}

/// Normalise an API endpoint: drop scheme and host, the leading slash, and the query string.
pub fn normalize_endpoint(endpoint: &str) -> String {
    let mut e = endpoint.trim();
    for scheme in ["https://", "http://"] {
        if let Some(after) = e.strip_prefix(scheme) {
            e = after.find('/').map(|i| &after[i..]).unwrap_or("");
        }
    }
    let e = e.trim_start_matches('/');
    let e = e.split(['?', '#']).next().unwrap_or("");
    e.to_string()
}

/// Parsed `gh api` arguments.
#[derive(Debug, Default, Clone)]
pub struct ApiArgs {
    /// Explicit `-X/--method` value.
    pub method: Option<String>,
    /// `-f/--raw-field key=value` pairs, verbatim.
    pub raw_fields: Vec<String>,
    /// `-F/--field key=value` pairs, verbatim (`@file` and `@-` read a file or stdin).
    pub typed_fields: Vec<String>,
    /// `--input FILE` (`-` = stdin).
    pub input: Option<String>,
    /// `--paginate`, as gh resolves it (`--paginate=false` and a later false value turn it off).
    pub paginate: bool,
    /// `--slurp`, resolved the same way.
    pub slurp: bool,
    /// `-i/--include`, resolved the same way (`-i=false` is off).
    pub include: bool,
    /// Positional arguments; the first is the endpoint.
    pub positionals: Vec<String>,
    /// First flag the parser did not recognise.
    pub unknown: Option<String>,
}

const API_VALUE_SHORT: &[char] = &['X', 'H', 'f', 'F', 'q', 't', 'p'];
const API_BOOL_SHORT: &[char] = &['i', 'h'];
const API_VALUE_LONG: &[&str] = &[
    "method",
    "header",
    "raw-field",
    "field",
    "jq",
    "template",
    "preview",
    "input",
    "cache",
    "hostname",
];
const API_BOOL_LONG: &[&str] = &["include", "paginate", "silent", "slurp", "verbose", "help"];

fn api_store(a: &mut ApiArgs, name: &str, value: String) {
    match name {
        "X" | "method" => a.method = Some(value.to_ascii_uppercase()),
        "f" | "raw-field" => a.raw_fields.push(value),
        "F" | "field" => a.typed_fields.push(value),
        "input" => a.input = Some(value),
        _ => {}
    }
}

/// Parse the arguments that follow `gh api` with pflag semantics: `--flag value` consumes the
/// next argument even when it starts with `-`, short flags combine (`-iXPOST`), and `--` ends
/// flag parsing.
pub fn parse_api(rest: &[String]) -> ApiArgs {
    let mut a = ApiArgs::default();
    let mut i = 0;
    while i < rest.len() {
        let t = rest[i].as_str();
        i += 1;
        if t == "--" {
            a.positionals.extend(rest[i..].iter().cloned());
            break;
        }
        if let Some(long) = t.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            if API_VALUE_LONG.contains(&name) {
                let value = match inline {
                    Some(v) => v,
                    None => {
                        let v = rest.get(i).cloned().unwrap_or_default();
                        i += 1;
                        v
                    }
                };
                api_store(&mut a, name, value);
            } else if API_BOOL_LONG.contains(&name) {
                // pflag: `--flag` is true, `--flag=VALUE` is VALUE, and the last one wins. A
                // value gh cannot read as a boolean counts as true (gh rejects it anyway).
                let on = inline
                    .as_deref()
                    .is_none_or(|v| crate::guard::parse_go_bool(v) != Some(false));
                match name {
                    "paginate" => a.paginate = on,
                    "slurp" => a.slurp = on,
                    "include" => a.include = on,
                    _ => {}
                }
            } else if a.unknown.is_none() {
                a.unknown = Some(t.to_string());
            }
            continue;
        }
        if is_flag(t) {
            let chars: Vec<char> = t[1..].chars().collect();
            let mut j = 0;
            while j < chars.len() {
                let c = chars[j];
                if API_VALUE_SHORT.contains(&c) {
                    let attached: String = chars[j + 1..].iter().collect();
                    let value = if attached.is_empty() {
                        let v = rest.get(i).cloned().unwrap_or_default();
                        i += 1;
                        v
                    } else {
                        attached.strip_prefix('=').unwrap_or(&attached).to_string()
                    };
                    api_store(&mut a, &c.to_string(), value);
                    break;
                } else if API_BOOL_SHORT.contains(&c) {
                    // pflag gives a boolean shorthand followed by `=` the rest of the token as
                    // its value (`-i=false`); otherwise the shorthand is true.
                    let inline: Option<String> =
                        (chars.get(j + 1) == Some(&'=')).then(|| chars[j + 2..].iter().collect());
                    let on = inline
                        .as_deref()
                        .is_none_or(|v| crate::guard::parse_go_bool(v) != Some(false));
                    if c == 'i' {
                        a.include = on;
                    }
                    if inline.is_some() {
                        break;
                    }
                    j += 1;
                } else {
                    if a.unknown.is_none() {
                        a.unknown = Some(t.to_string());
                    }
                    break;
                }
            }
            continue;
        }
        a.positionals.push(t.to_string());
    }
    a
}

/// A flag's name without its value: `--token=X` is `--token`, `-tX` is `-t`.
fn flag_name(flag: &str) -> String {
    if let Some(long) = flag.strip_prefix("--") {
        format!("--{}", long.split('=').next().unwrap_or(""))
    } else {
        flag.chars().take(2).collect()
    }
}

/// True when `text` contains `word` delimited by non-identifier characters.
pub fn contains_word(text: &str, word: &str) -> bool {
    let bytes = text.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0;
    while let Some(pos) = text[start..].find(word) {
        let at = start + pos;
        let end = at + word.len();
        let before_ok = at == 0 || !is_ident(bytes[at - 1]);
        let after_ok = end >= bytes.len() || !is_ident(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        start = at + 1;
    }
    false
}

/// Largest file the classifier reads to inspect a GraphQL document.
pub const MAX_INSPECT_BYTES: u64 = 1 << 20;

/// Read a regular file of at most [`MAX_INSPECT_BYTES`] as UTF-8; anything else is `None`.
fn read_small(path: &str) -> Option<String> {
    use std::io::Read;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_INSPECT_BYTES {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut buf = String::new();
    file.take(MAX_INSPECT_BYTES + 1)
        .read_to_string(&mut buf)
        .ok()?;
    if buf.len() as u64 > MAX_INSPECT_BYTES {
        return None;
    }
    Some(buf)
}

/// Every GraphQL document a `gh api graphql` call may send, or `None` when any part of the
/// request body cannot be inspected without consuming stdin.
///
/// With `--input`, gh sends that file as the request body and moves the fields into the URL; without
/// it, the fields form the body. Rather than model which one GitHub reads, every candidate is
/// collected and the call is READ only when none of them is a mutation:
///
/// - every `-f/-F query=...` value (a typed `@file` is read; `@-` is uninspectable);
/// - a key such as `query[...]`, which gh nests into an object, is uninspectable;
/// - `--input FILE` must be a JSON object with a string `query`; its raw text is also returned so
///   the word `mutation` anywhere in it (a second operation, an escaped name) counts;
/// - `--input -` is uninspectable.
fn graphql_documents(a: &ApiArgs) -> Option<Vec<String>> {
    let mut docs = Vec::new();
    for f in &a.raw_fields {
        let (key, value) = f.split_once('=').unwrap_or((f.as_str(), ""));
        if key == "query" {
            docs.push(value.to_string());
        } else if key.starts_with("query[") {
            return None;
        }
    }
    for f in &a.typed_fields {
        let (key, value) = f.split_once('=').unwrap_or((f.as_str(), ""));
        if key == "query" {
            match value.strip_prefix('@') {
                Some("-") => return None,
                Some(path) => docs.push(read_small(path)?),
                None => docs.push(value.to_string()),
            }
        } else if key.starts_with("query[") {
            return None;
        }
    }
    if let Some(input) = &a.input {
        if input == "-" {
            return None;
        }
        let text = read_small(input)?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        let query = value.as_object()?.get("query")?.as_str()?.to_string();
        docs.push(query);
        docs.push(text);
    }
    Some(docs)
}

/// True when `endpoint` (already normalised) is a GraphQL endpoint.
fn is_graphql(endpoint: &str) -> bool {
    endpoint == "graphql" || endpoint.ends_with("/graphql")
}

fn classify_api(rest: &[String], cfg: &Config) -> Classification {
    let a = parse_api(rest);
    let endpoint = normalize_endpoint(a.positionals.first().map(String::as_str).unwrap_or(""));
    let has_body = !a.raw_fields.is_empty() || !a.typed_fields.is_empty() || a.input.is_some();
    let method = a.method.clone().unwrap_or_else(|| {
        if has_body {
            "POST".into()
        } else {
            "GET".into()
        }
    });
    let paginate = a.paginate || a.slurp;
    let api = ApiCall {
        method: method.clone(),
        endpoint: endpoint.clone(),
        paginate,
        include: a.include,
    };
    if let Some(flag) = &a.unknown {
        // The unknown flag may take a value, so the method and the endpoint gh-paced read may be
        // that value (`api --token SECRET repos/o/r` reads `SECRET` as the endpoint). Neither is
        // kept: the command recorded in the audit log and the state file is `api <unparsed>`.
        let mut c = paced(
            Class::Write,
            1,
            "api",
            None,
            "api <unparsed>".into(),
            &format!(
                "unrecognised gh api flag {}; classified WRITE (fail safe)",
                flag_name(flag)
            ),
        );
        c.api = Some(ApiCall {
            method: "<unparsed>".into(),
            endpoint: "<unparsed>".into(),
            ..api
        });
        charge_pagination(&mut c, paginate, cfg);
        return c;
    }
    let command = format!("api {method} {endpoint}").trim().to_string();
    let mut c = paced(Class::Write, 1, "api", None, command, "");
    c.api = Some(api);
    let read_method = matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS");
    if is_graphql(&endpoint) {
        // Inspected for every method: `-X GET graphql -f query=mutation{...}` is still a
        // mutation as far as this wrapper is concerned.
        match graphql_documents(&a) {
            Some(docs) if docs.is_empty() => {
                c.reason =
                    "GraphQL request with no query to inspect; classified WRITE (fail safe)".into()
            }
            Some(docs) if docs.iter().all(|d| !contains_word(d, "mutation")) => {
                c.class = Class::Read;
                c.reason = "GraphQL query without a mutation".into();
            }
            Some(_) => c.reason = "GraphQL mutation".into(),
            None => {
                c.reason =
                    "GraphQL request whose body cannot be inspected; classified WRITE (fail safe)"
                        .into()
            }
        }
    } else if read_method {
        if endpoint.starts_with("search/") {
            c.class = Class::Search;
            c.reason = format!("{method} on the search API");
        } else if endpoint == "rate_limit" {
            c.class = Class::Read;
            c.reason =
                "GET /rate_limit (free on the primary limit, counted by secondary limits)".into();
        } else {
            c.class = Class::Read;
            c.reason = format!("{method} request");
        }
    } else if a.method.is_some() {
        c.reason = format!("explicit {method} request");
    } else {
        c.reason = "fields or --input without -X GET make gh send a POST".into();
    }
    charge_pagination(&mut c, paginate, cfg);
    c
}

/// Charge `paginate_cost` tokens for `--paginate` or `--slurp`, on every classification path.
///
/// A paginated write (a GraphQL mutation with a cursor, a POST that follows Link headers) can
/// repeat the write once per page, so it pays the same N tokens as a read.
fn charge_pagination(c: &mut Classification, paginate: bool, cfg: &Config) {
    if paginate {
        c.cost = c.cost.max(cfg.paginate_cost.max(1));
        c.warnings.push(format!(
            "--paginate fetches pages back to back; charging {} {} tokens for this one invocation",
            c.cost,
            c.class.name()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cls(line: &str) -> Classification {
        let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        classify(&args, &Config::default())
    }

    /// `cls` with watches allowed (`GH_PACED_ALLOW_WATCH=1`), so the interval checks show.
    fn clsw(line: &str) -> Classification {
        let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        let cfg = Config {
            allow_watch: true,
            ..Config::default()
        };
        classify(&args, &cfg)
    }

    fn class_of(line: &str) -> Class {
        cls(line).class
    }

    #[test]
    fn local_commands() {
        for line in [
            "",
            "--version",
            "version",
            "help",
            "help pr create",
            "help environment",
            "pr --help",
            "pr create --help",
            "api --help",
            "pr comment 12 -h",
            "completion -s bash",
            "config get git_protocol",
            "config set editor vim",
            "alias list",
            "alias set co 'pr checkout'",
            "auth token",
            "auth setup-git",
            "auth switch --user someone",
            "auth git-credential store",
            "auth git-credential erase",
            "environment",
            "exit-codes",
            "formatting",
            "reference",
            "actions",
            "mintty",
            "accessibility",
        ] {
            assert_eq!(class_of(line), Class::Local, "{line:?}");
        }
    }

    #[test]
    fn help_detection_is_not_fooled_by_a_flag_value() {
        // `-b --help` makes `--help` the body, so gh would post a comment.
        assert_eq!(class_of("pr comment 12 -b --help"), Class::Write);
        // `-h` that is not the last token is not treated as help.
        assert_eq!(class_of("pr comment -h 12 -b x"), Class::Write);
        // An alias or extension receives `--help` as an argument and may still write.
        assert_eq!(class_of("my-alias --help"), Class::Write);
        assert_eq!(class_of("my-alias 12 --help"), Class::Write);
        assert_eq!(class_of("some-extension -h"), Class::Write);
        assert_eq!(cls("my-alias --help").command, "my-alias");
        // `gh extension exec` hands `--help` to the extension, which may ignore it and write.
        assert_eq!(class_of("extension exec foo --help"), Class::Write);
        assert_eq!(class_of("extension exec foo -h"), Class::Write);
        assert_eq!(class_of("extension exec --help"), Class::Write);
        // gh's own extension subcommands still print help locally.
        assert_eq!(class_of("extension list --help"), Class::Local);
        assert_eq!(class_of("extension --help"), Class::Local);
    }

    #[test]
    fn compound_aliases_and_passthrough_commands_are_opaque() {
        // gh adds a user alias such as `issue publish` as a child of the group it extends, so a
        // word that is not one of the group's subcommands runs an alias, which receives `--help`
        // as an argument. cobra strips flags while it looks for the command, so `gh --help
        // publish` and `gh pr --help publish` run the aliases `publish` and `pr publish`.
        for line in [
            "issue publish --help",
            "issue publish -h",
            "issue -R o/r publish --help",
            "pr --help publish",
            "--help publish",
            "config publish x",
            "config publish --help",
            "alias publish",
            "environment publish",
            "mintty publish --help",
            "repo autolink publish --help",
            "extension exec my-ext --help",
            "ext exec my-ext",
            "copilot --help",
        ] {
            let c = cls(line);
            assert_eq!(c.class, Class::Write, "{line:?}");
            assert!(c.opaque, "{line:?}");
            assert!(is_unknown_command(&c), "{line:?}");
        }
        let c = cls("issue publish BODY words");
        assert_eq!(c.command, "issue publish");
        assert_eq!(c.rest_start, 2);
        let c = cls("extension exec my-ext --body=x");
        assert_eq!(c.command, "extension exec");
        assert_eq!(c.rest_start, 2);
        // gh's own commands are unchanged.
        for line in [
            "issue --help",
            "issue list --help",
            "config get editor",
            "config --help",
            "alias set co x",
            "environment",
            "pr",
            "repo autolink",
            "pr -R o/r",
        ] {
            let c = cls(line);
            assert_eq!(c.class, Class::Local, "{line:?}");
            assert!(!c.opaque, "{line:?}");
        }
        // A runnable command's argument is not an alias, even beneath a runnable group.
        let c = cls("codespace ports 8080");
        assert!(!c.opaque);
        assert_eq!(
            cls("codespace ports forward 1:2").command,
            "codespace ports forward"
        );
        assert!(!cls("pr view publish").opaque);
        // cobra's own aliases resolve to the command they name.
        assert_eq!(class_of("cs list"), Class::Read);
        assert_eq!(cls("rs view 1").command, "ruleset view");
        assert_eq!(cls("issue ls").command, "issue list");
        assert!(!cls("secret remove NAME").opaque);
    }

    #[test]
    fn read_commands() {
        for line in [
            "pr view 12",
            "pr list --state open",
            "pr ls",
            "pr status",
            "pr checks 12",
            "pr diff 12",
            "pr checkout 12",
            "pr co 12",
            "-R owner/repo pr view 12",
            "pr -R owner/repo list",
            "issue view 3",
            "issue list",
            "issue status",
            "run view 99",
            "run list",
            "run download 99",
            "workflow list",
            "workflow view ci.yml",
            "repo view",
            "repo list",
            "repo clone owner/repo",
            "repo gitignore list",
            "repo license view mit",
            "repo autolink list",
            "repo deploy-key list",
            "release list",
            "release view v1",
            "release download v1",
            "label list",
            "gist list",
            "gist view abc",
            "secret list",
            "variable list",
            "variable get NAME",
            "cache list",
            "ssh-key list",
            "gpg-key list",
            "org list",
            "project list",
            "project view 1",
            "ruleset list",
            "ruleset check",
            "attestation verify file",
            "auth status",
            "browse 12",
            "extension list",
            "api repos/owner/repo",
            "api /repos/owner/repo/pulls?state=open",
            "api -X GET repos/owner/repo/issues -f state=open",
            "api --method=get repos/owner/repo",
            "api -XGET repos/o/r/issues -F per_page=100",
            "api -X HEAD repos/o/r",
            "api https://api.github.com/repos/o/r",
            "api rate_limit",
            "api graphql -f query={viewer{login}}",
        ] {
            assert_eq!(class_of(line), Class::Read, "{line:?}");
        }
    }

    #[test]
    fn search_commands() {
        for line in [
            "search issues foo",
            "search prs --repo o/r bar",
            "search code baz",
            "search repos x",
            "search commits y",
            "api search/issues?q=foo",
            "api -X GET search/code -f q=bar",
            "extension search thing",
        ] {
            assert_eq!(class_of(line), Class::Search, "{line:?}");
        }
        // `gh status` searches through GraphQL, like `gh api graphql`, so it is READ; it was
        // SEARCH/3, which the burst rule (cost above the SEARCH burst of 2) refused outright.
        let status = cls("status");
        assert_eq!((status.class, status.cost), (Class::Read, STATUS_COST));
        assert!(STATUS_COST as f64 <= Config::default().read.burst);
    }

    #[test]
    fn write_commands() {
        for line in [
            "api -X POST repos/o/r/issues/1/comments -f body=hi",
            "api --method PATCH repos/o/r/issues/1",
            "api --method=put repos/o/r/contents/x",
            "api -XDELETE repos/o/r/labels/x",
            "api repos/o/r/issues/1/comments -f body=hi",
            "api repos/o/r/issues/1/comments -F body=@file.md",
            "api repos/o/r/issues/1/comments --raw-field body=hi",
            "api repos/o/r/issues/1/comments --field body=hi",
            "api --method POST repos/o/r/issues/1/comments --input request.json",
            "api repos/o/r/issues/1/comments --input -",
            "api graphql -f query=mutation{addComment}",
            "api graphql -F query=@-",
            "api graphql --input -",
            "api --unknown-flag repos/o/r",
            "pr create --title t --body b",
            "pr new",
            "pr comment 1 -b hi",
            "pr edit 1 --title t",
            "pr merge 1 --squash",
            "pr close 1",
            "pr reopen 1",
            "pr review 1 --approve",
            "pr ready 1",
            "pr lock 1",
            "pr update-branch 1",
            "issue create -t t -b b",
            "issue comment 1 -b hi",
            "issue edit 1 --add-label x",
            "issue close 1",
            "issue reopen 1",
            "issue delete 1",
            "issue transfer 1 o/r",
            "issue lock 1",
            "issue pin 1",
            "label create bug",
            "label edit bug",
            "label delete bug",
            "label clone o/r",
            "release create v1",
            "release edit v1",
            "release delete v1",
            "release upload v1 file",
            "workflow run ci.yml",
            "workflow enable ci.yml",
            "workflow disable ci.yml",
            "run rerun 1",
            "run cancel 1",
            "run delete 1",
            "repo create x",
            "repo edit --description d",
            "repo delete o/r",
            "repo fork o/r",
            "repo sync",
            "repo rename x",
            "repo archive o/r",
            "repo autolink create",
            "repo deploy-key add key.pub",
            "gist create file.txt",
            "gist edit abc",
            "gist delete abc",
            "secret set NAME",
            "secret delete NAME",
            "variable set NAME",
            "variable delete NAME",
            "cache delete key",
            "ssh-key add k.pub",
            "gpg-key delete 1",
            "project create",
            "project item-add 1",
            "auth login",
            "auth logout",
            "auth refresh",
            "auth git-credential",
            "extension install o/gh-x",
            "codespace create",
            "my-alias",
            "some-extension arg",
            "--unknown pr view",
            "pr --state open list",
            "copilot suggest x",
        ] {
            assert_eq!(class_of(line), Class::Write, "{line:?}");
        }
    }

    #[test]
    fn git_credential_get() {
        assert_eq!(class_of("auth git-credential get"), Class::GitCredential);
        assert_eq!(
            cls("auth git-credential get").command,
            "auth git-credential get"
        );
    }

    #[test]
    fn paginate_and_limit_costs() {
        let c = cls("api --paginate repos/o/r/issues");
        assert_eq!((c.class, c.cost), (Class::Read, 10));
        assert!(!c.warnings.is_empty());
        let c = cls("api --slurp --paginate repos/o/r/issues");
        assert_eq!(c.cost, 10);
        let c = cls("api -X GET --paginate search/issues -f q=x");
        assert_eq!((c.class, c.cost), (Class::Search, 10));
        assert_eq!(cls("pr list -L 1000").cost, 10);
        assert_eq!(cls("pr list --limit=250").cost, 3);
        assert_eq!(cls("pr list -L50").cost, 1);
        assert_eq!(cls("search issues x --limit 300").cost, 3);
        // A paginated write can repeat once per page, so it pays the same N tokens as a read.
        let c = cls("api --paginate -X POST repos/o/r/x");
        assert_eq!((c.class, c.cost), (Class::Write, 10));
        assert!(!c.warnings.is_empty());
        let c = cls("api graphql --paginate -f query=mutation{x}");
        assert_eq!((c.class, c.cost), (Class::Write, 10));
        // An unrecognised flag makes the call WRITE, and must not drop the pagination charge.
        // Its value may have been read as the method or endpoint, so neither is kept.
        for line in [
            "api --token CANARY repos/o/r",
            "api --token=CANARY repos/o/r",
            "api -ZCANARY repos/o/r",
            "api --token -X CANARY repos/o/r",
        ] {
            let c = cls(line);
            assert_eq!(
                (c.class, c.command.as_str()),
                (Class::Write, "api <unparsed>"),
                "{line:?}"
            );
            let api = c.api.as_ref().unwrap();
            for text in [&c.command, &c.reason, &api.method, &api.endpoint] {
                assert!(!text.contains("CANARY"), "{line:?}: {text:?}");
            }
        }
        assert!(!cls("--token=CANARY pr list").reason.contains("CANARY"));
        assert!(!cls("pr --token=CANARY list").reason.contains("CANARY"));
        for line in [
            "api --allow-escape-sequences --paginate -X POST repos/o/r/x",
            "api --paginate --allow-escape-sequences -X POST repos/o/r/x",
            "api -Z --slurp --paginate repos/o/r/issues",
        ] {
            let c = cls(line);
            assert_eq!((c.class, c.cost), (Class::Write, 10), "{line:?}");
            assert!(!c.warnings.is_empty(), "{line:?}");
        }
    }

    /// gh honours the last occurrence of a scalar flag; gh-paced takes the most conservative of
    /// all occurrences, so neither order escapes the charge or the refusal.
    #[test]
    fn repeated_flags_use_the_most_conservative_value() {
        assert_eq!(cls("pr list --limit 1 --limit 1000").cost, 10);
        assert_eq!(cls("pr list --limit 1000 --limit 1").cost, 10);
        assert_eq!(cls("pr list -L 1 --limit=1000").cost, 10);
        assert!(clsw("run watch 1 --interval 30 --interval 1")
            .refusal
            .is_some());
        assert!(clsw("run watch 1 -i 1 --interval 30").refusal.is_some());
        assert!(clsw("pr checks 1 --watch --interval 60 -i 2")
            .refusal
            .is_some());
        assert!(clsw("run watch 1 -i 30 --interval 45").refusal.is_none());
    }

    /// gh sleeps `interval` seconds and honours the last value; zero or a negative value means
    /// no sleep at all, so any such value is refused wherever it appears.
    #[test]
    fn non_positive_or_unreadable_intervals_are_refused() {
        for line in [
            "run watch 1 --interval 30 --interval -1",
            "run watch 1 --interval -1 --interval 30",
            "run watch 1 -i 30 -i -1",
            "run watch 1 -i30 -i=-5",
            "run watch 1 --interval=0",
            "run watch 1 --interval 30s",
            "run watch 1 --interval 0x1e",
            "run watch 1 --interval 036",
            "run watch 1 --interval=+30",
            "pr checks 1 --watch --interval 60 --interval -1",
            "pr checks 1 --watch -i -1",
        ] {
            let c = clsw(line);
            assert!(c.refusal.is_some(), "{line:?} must be refused");
        }
        // The fast-watch override allows a short interval, never a non-positive one.
        let cfg = Config {
            allow_fast_watch: true,
            allow_watch: true,
            ..Config::default()
        };
        let argv = |line: &str| -> Vec<String> { line.split(' ').map(String::from).collect() };
        assert!(classify(&argv("run watch 1 -i 30 -i -1"), &cfg)
            .refusal
            .is_some());
        assert!(classify(&argv("run watch 1 -i 0"), &cfg).refusal.is_some());
        let c = classify(&argv("run watch 1 -i 1"), &cfg);
        assert!(c.refusal.is_none());
        assert_eq!(c.deadline_secs, Some(4.0));
    }

    /// pflag reads boolean shorthands in a group one letter at a time until a value shorthand
    /// takes the rest of the token, so `-dL1000` is `--draft --limit 1000`.
    #[test]
    fn grouped_shorthands_are_read() {
        assert_eq!(cls("pr list --limit 1 -dL1000").cost, 10);
        assert_eq!(cls("pr list -dL 1000").cost, 10);
        assert_eq!(cls("run list -aL1000").cost, 10);
        assert_eq!(cls("run list -aL=1000").cost, 10);
        assert!(clsw("pr checks 1 --watch -i 60 -wi1").refusal.is_some());
        assert!(clsw("run watch 1 --interval 60 -xi -1").refusal.is_some());
        // `-R` takes the rest of the token as the repository, so `L` there is not a flag.
        assert_eq!(cls("pr list -RL1000/x").cost, 1);
        // `=` gives the rest of the token to the letter before it.
        assert_eq!(cls("pr list -d=L1000").cost, 1);
        assert_eq!(
            short_group_value("dL1000", 'L'),
            GroupValue::Attached("1000")
        );
        assert_eq!(short_group_value("dL", 'L'), GroupValue::Next);
        assert_eq!(short_group_value("RL5", 'L'), GroupValue::Absent);
        assert_eq!(short_group_value("d", 'L'), GroupValue::Absent);
        // A token read as another flag's value is still examined as a flag: gh reads `-dL` as
        // the label here, and then sees `--limit 1000`.
        assert_eq!(cls("pr list --label -dL --limit 1000").cost, 10);
        assert_eq!(cls("pr list --search -L -L 1000").cost, 10);
        assert_eq!(cls("pr list --limit --limit=1000").cost, 10);
    }

    /// pflag reads integers with Go's base-prefix rules, so `--limit=0x3e8` is 1000.
    #[test]
    fn limit_values_are_read_as_gh_reads_them() {
        for line in [
            "pr list --limit=0x3e8",
            "pr list -L 0X3E8",
            "pr list --limit 0o1750",
            "pr list --limit 01750",
            "pr list --limit 0b1111101000",
            "pr list --limit 1_000",
            "pr list --limit +1000",
            "pr list -L0x3e8",
        ] {
            assert_eq!(cls(line).cost, 10, "{line:?}");
        }
        // gh rejects a negative or unreadable limit before any request.
        assert_eq!(cls("pr list --limit=-5000").cost, 1);
        assert_eq!(cls("pr list --limit=lots").cost, 1);
        assert_eq!(parse_go_int("0x3e8"), Some(1000));
        assert_eq!(parse_go_int("-0b11"), Some(-3));
        assert_eq!(parse_go_int("010"), Some(8));
        assert_eq!(parse_go_int("0"), Some(0));
        assert_eq!(parse_go_int("0x"), None);
        assert_eq!(parse_go_int("12z"), None);
        // Far beyond u64: still the most conservative charge, not dropped.
        assert_eq!(
            cls("pr list --limit 99999999999999999999999").cost,
            u32::MAX
        );
    }

    #[test]
    fn watch_loops_are_charged_and_fast_ones_refused() {
        let c = clsw("pr checks 12 --watch");
        assert_eq!(c.cost, 20);
        assert!(c.refusal.is_some(), "default 10 s interval is too fast");
        let c = clsw("pr checks 12 --watch --interval 60");
        assert!(c.refusal.is_none());
        assert_eq!(c.cost, 20);
        let c = clsw("run watch 99");
        assert!(c.refusal.is_some(), "default 3 s interval is too fast");
        let c = clsw("run watch 99 -i 30");
        assert!(c.refusal.is_none());
        // 20 tokens: 2 for startup, then 4 polls of 4 requests each, 30 s apart: 120 s.
        assert_eq!(c.deadline_secs, Some(120.0));
        // 20 tokens: 2 for startup, then 9 polls of 2 requests each, 30 s apart: 270 s.
        assert_eq!(
            clsw("pr checks 12 --watch --interval 30").deadline_secs,
            Some(270.0)
        );
        let c = cls("pr checks 12");
        assert_eq!(c.cost, 1);
        assert!(c.refusal.is_none());
        assert_eq!(c.deadline_secs, None);
        // `--watch` is a boolean: gh honours its value and its last occurrence.
        for line in [
            "pr checks 12 --watch=false",
            "pr checks 12 --watch=0",
            "pr checks 12 --watch --watch=F",
        ] {
            let c = cls(line);
            assert_eq!((c.cost, c.deadline_secs), (1, None), "{line:?}");
            assert!(c.refusal.is_none(), "{line:?}");
        }
        for line in [
            "pr checks 12 --watch=false --watch",
            "pr checks 12 --watch=true",
            "pr checks 12 --watch=maybe",
        ] {
            let c = clsw(line);
            assert_eq!(c.cost, 20, "{line:?}");
            assert!(
                c.refusal.is_some(),
                "{line:?}: default interval is too fast"
            );
        }
        assert_eq!(cls("pr view 12").deadline_secs, None);
        let cfg = Config {
            allow_fast_watch: true,
            allow_watch: true,
            ..Config::default()
        };
        let args: Vec<String> = ["run", "watch", "1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(classify(&args, &cfg).refusal.is_none());
    }

    #[test]
    fn api_parser_follows_pflag_rules() {
        let args: Vec<String> = ["-iXPOST", "repos/o/r", "-f", "-body=x", "--", "--input"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = parse_api(&args);
        assert_eq!(a.method.as_deref(), Some("POST"));
        assert!(a.include, "-i in a combined short group");
        assert_eq!(a.raw_fields, vec!["-body=x".to_string()]);
        assert_eq!(
            a.positionals,
            vec!["repos/o/r".to_string(), "--input".to_string()]
        );
        assert!(a.input.is_none());
        let args: Vec<String> = ["-H", "-X", "repos/o/r"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = parse_api(&args);
        assert!(a.method.is_none(), "-X here is the value of -H");
        assert!(!a.include);
        assert!(cls("api --include repos/o/r").api.expect("api").include);
        assert!(!cls("api repos/o/r").api.expect("api").include);
        assert!(
            !cls("api -H -i repos/o/r").api.expect("api").include,
            "-i here is the value of -H"
        );
        assert_eq!(normalize_endpoint("/repos/o/r?x=1"), "repos/o/r");
        assert_eq!(
            normalize_endpoint("https://api.github.com/graphql"),
            "graphql"
        );
    }

    #[test]
    fn graphql_query_files_are_inspected() {
        let dir = std::env::temp_dir().join(format!("gh-paced-gql-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let q = dir.join("q.graphql");
        std::fs::write(&q, "query { viewer { login } }").unwrap();
        let m = dir.join("m.graphql");
        std::fs::write(
            &m,
            "mutation { addComment(input: {}) { clientMutationId } }",
        )
        .unwrap();
        let inp = dir.join("in.json");
        std::fs::write(&inp, r#"{"query": "mutation X { y }"}"#).unwrap();
        let line = |extra: &str| -> Class {
            let mut args = vec!["api".to_string(), "graphql".to_string()];
            args.extend(extra.split_whitespace().map(str::to_string));
            classify(&args, &Config::default()).class
        };
        assert_eq!(line(&format!("-F query=@{}", q.display())), Class::Read);
        assert_eq!(line(&format!("-F query=@{}", m.display())), Class::Write);
        assert_eq!(line(&format!("--input {}", inp.display())), Class::Write);
        assert_eq!(line("-F query=@/nonexistent/file"), Class::Write);
        // gh sends the --input file as the request body (fields move to the URL), so a query
        // field cannot vouch for it: stdin cannot be inspected, and a mutation file is a write.
        assert_eq!(
            line("-f query=query{viewer{login}} --input -"),
            Class::Write
        );
        assert_eq!(
            line(&format!("-f query=query{{x}} --input {}", inp.display())),
            Class::Write
        );
        let qin = dir.join("qin.json");
        std::fs::write(&qin, r#"{"query": "query { viewer { login } }"}"#).unwrap();
        assert_eq!(line(&format!("--input {}", qin.display())), Class::Read);
        // Every query value counts, whichever gh keeps.
        assert_eq!(line("-f query=query{x} -f query=mutation{y}"), Class::Write);
        assert_eq!(line("-f query=mutation{y} -f query=query{x}"), Class::Write);
        // GraphQL is inspected whatever the method.
        assert_eq!(line("-X GET -f query=mutation{y}"), Class::Write);
        assert_eq!(line("-X GET -f query=query{y}"), Class::Read);
        // No query at all: nothing to vouch for a read.
        assert_eq!(line(""), Class::Write);
        assert!(contains_word("mutation{x}", "mutation"));
        assert!(!contains_word("query { mutations }", "mutation"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// gh-paced cannot count the requests a watch makes, so a watch is refused unless
    /// `GH_PACED_ALLOW_WATCH=1` accepts the estimate. The charge and deadline are still worked
    /// out so the refusal can name them; a plain `pr checks` is untouched.
    #[test]
    fn watches_need_an_explicit_opt_in() {
        for line in [
            "pr checks 12 --watch --interval 60",
            "run watch 99 -i 30",
            "run watch 99",
        ] {
            let c = cls(line);
            let refusal = c.refusal.as_deref().unwrap_or_default();
            assert!(
                refusal.contains("GH_PACED_ALLOW_WATCH=1"),
                "{line:?}: {refusal}"
            );
            assert!(
                refusal.contains("cannot count the requests"),
                "{line:?}: {refusal}"
            );
            assert_eq!(c.cost, 20, "{line:?}");
            assert!(c.deadline_secs.is_some(), "{line:?}");
            assert!(clsw(&format!("{line} -i 30")).refusal.is_none(), "{line:?}");
        }
        // A too-fast interval is reported alongside, so one retry fixes both.
        let refusal = cls("run watch 99").refusal.unwrap();
        assert!(
            refusal.contains("faster than the 30 s minimum"),
            "{refusal}"
        );
        let c = cls("pr checks 12");
        assert_eq!((c.cost, c.deadline_secs, c.refusal), (1, None, None));
    }

    /// A `--` can be another flag's value (`--label --`), and gh then reads the flags after it.
    /// gh-paced does not know which flags take a value, so it reads past every `--`.
    #[test]
    fn a_double_dash_flag_value_does_not_hide_later_flags() {
        assert_eq!(cls("pr list --label -- --limit 1000").cost, 10);
        assert_eq!(cls("pr list --search -- -L1000").cost, 10);
        assert_eq!(cls("issue list --label -- --limit=250").cost, 3);
        // `--repo --` takes the `--`, so `--watch` after it is a real flag.
        let c = cls("pr checks 12 --repo -- --watch --interval 60");
        assert_eq!(c.cost, 20);
        assert!(c.deadline_secs.is_some());
        // A watch that is on before a `--` stays on whatever follows, since that `--` may end
        // the flags.
        let c = cls("pr checks 12 --watch --interval 60 -- --watch=false");
        assert!(c.deadline_secs.is_some());
        // Off everywhere is still off.
        let c = cls("pr checks 12 -- --watch=false");
        assert_eq!((c.cost, c.deadline_secs), (1, None));
    }

    /// pflag resolves `--paginate=false`, `--include=false` and `-i=false` to false, and the
    /// last occurrence wins.
    #[test]
    fn api_booleans_honour_explicit_false_values() {
        let args = |line: &str| -> Vec<String> { line.split(' ').map(String::from).collect() };
        for line in [
            "api --paginate=false repos/o/r/issues",
            "api --paginate --paginate=false repos/o/r/issues",
            "api --paginate=0 --slurp=false repos/o/r/issues",
        ] {
            let c = cls(line);
            assert_eq!((c.class, c.cost), (Class::Read, 1), "{line:?}");
        }
        assert_eq!(
            cls("api --paginate=false --paginate repos/o/r/issues").cost,
            10
        );
        assert_eq!(cls("api --paginate=maybe repos/o/r/issues").cost, 10);
        assert!(!parse_api(&args("--include=false repos/o/r")).include);
        assert!(!parse_api(&args("--include --include=false repos/o/r")).include);
        assert!(parse_api(&args("--include=false --include repos/o/r")).include);
        let a = parse_api(&args("-i=false repos/o/r"));
        assert!(!a.include);
        assert_eq!(a.unknown, None);
        assert_eq!(a.positionals, ["repos/o/r"]);
        let c = cls("api -i=false repos/o/r");
        assert_eq!((c.class, c.cost), (Class::Read, 1));
        assert!(parse_api(&args("-i=true repos/o/r")).include);
        assert!(parse_api(&args("-i repos/o/r")).include);
        // `-hi=false`: help, then include false.
        assert!(!parse_api(&args("-hi=false repos/o/r")).include);
    }
}
