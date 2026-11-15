#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! ```cargo
//! [dependencies]
//! serde_json = "1"
//! ```
//!
//! Per-syscall Detcore pages: lint, facts, aggregate, check, and the one-time
//! conversion of the recovered `syscall-tracking/v1` bodies into pages.
//!
//! The pages in `docs/syscalls/<name>.md` are the source of truth for what
//! Detcore does with each Linux syscall and why: its determinization
//! strategy (modelled, emulated, refused with a fixed errno, passed
//! through, or unsupported). Which backends can run a syscall is not tracked
//! here; that is a Reverie/backend question. Test coverage (which syscalls
//! the validate matrix exercises) is likewise separate: the generated index
//! links to it and never merges its numbers into the strategy table. The
//! normative page format is `docs/syscalls/SCHEMA.md`.
//!
//! Subcommands:
//! - `lint` — schema, sections, flag/test inventories, Status format, the
//!   public-text scrub, and discussion-link shape. No build, runs in
//!   seconds.
//! - `facts [--apply]` — export per-syscall policy from Detcore itself
//!   (public API only, through `scripts/syscall-pages-facts.rs`) plus the
//!   dispatch parse and the tests-naming count. Bare `facts` prints a diff
//!   against the pages' `code_facts`; `--apply` rewrites those blocks.
//! - `aggregate [--check]` — regenerate `docs/syscalls/syscalls.json` (the
//!   machine-readable strategy table a website indexes) and the counts and
//!   table in `docs/syscalls/README.md` from the pages.
//! - `check [--quick]` — lint + census + generated-file freshness + the
//!   dispatch parse; full `check` also compares every page's `code_facts`
//!   against a facts export at HEAD. `--quick` skips that build and takes
//!   each page's class from its own front matter, so it proves
//!   self-consistency and that every page claiming `determinized` has an
//!   explicit dispatch arm in today's source, but it cannot prove the
//!   claimed class is today's. Only full `check` proves that.
//! - `convert --from DIR --mapping JSON --facts TSV --sha SHA` — the
//!   one-time import of the recovered v1 bodies into page form. The input
//!   bodies are not in this repository; the converted pages are.
//!
//! What the dispatch parse cannot understand is `parser-failed`, never a
//! pass. What `lint` cannot parse is an error, never a skip.
//!
//! Not yet ported from the dev-hermit predecessor tool: the C08 rule that
//! resolves a recorded `T<n> covered by <path>` token to a strict-container
//! ptrace manifest cell or hermit-cli integration test. This tool checks
//! token shape, that the named test ids exist, and that every cited path
//! exists in the tree; it does not yet prove the cited test runs Hermit in
//! the default strict container. That gap is recorded in SCHEMA.md so a
//! `done` claim is never read as C08-verified by this tool alone.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::exit;

const SCHEMA: &str = "syscall-pages/v1";
const PAGES_DIR: &str = "docs/syscalls";
const JSON_FILE: &str = "docs/syscalls/syscalls.json";
const INDEX_FILE: &str = "docs/syscalls/README.md";
/// Headings of a page body, in order. The v1 `## Tracking data` section
/// became YAML front matter; `## Discussion` is new in page form.
const SECTIONS: [&str; 9] = [
    "Summary",
    "Linux semantics",
    "Determinism analysis",
    "Hermit design",
    "Recommended tests",
    "Done criteria",
    "Status",
    "Discussion",
    "Additional information",
];
const LINUX_DETERMINISM: [&str; 3] = ["deterministic", "conditional", "nondeterministic"];
const TARGET_POLICY: [&str; 5] = ["model", "emulate", "refuse", "passthrough", "investigate"];
const TRACKING_STATUS: [&str; 5] = ["todo", "in-progress", "blocked", "done", "wontfix"];
const DETCORE_CLASS: [&str; 3] = ["determinized", "passthrough", "unsupported"];

fn repo_root() -> PathBuf {
    let script = Path::new(file!());
    script
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn fail(msg: &str) -> ! {
    eprintln!("syscall-pages: {msg}");
    exit(2)
}

fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", path.display())))
}

fn is_syscall_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
}

// ---------------------------------------------------------------------------
// Page model. Front matter is a fixed YAML subset: scalar keys, one string
// list, and two nested blocks. The parser understands exactly that subset
// and rejects everything else, so a hand edit outside the subset is a lint
// error rather than a silently different reading.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct CodeFacts {
    as_of: String,
    detcore_class: String,
    dispatch: String,
    refused: bool,
    refused_only_when_fail_closed: bool,
    subscribed_run: bool,
    subscribed_record_replay: bool,
    tests_naming: u32,
}

#[derive(Debug, Clone, PartialEq)]
struct Page {
    syscall: String,
    nr_x86_64: i32,
    linux_determinism: String,
    target_policy: String,
    tracking_status: String,
    cross_host_variant: bool,
    related: Vec<String>,
    discussion_kind: String,
    discussion_url: String,
    code_facts: CodeFacts,
    /// Section heading -> body text (without the heading line).
    sections: BTreeMap<String, String>,
}

struct Report {
    errors: Vec<String>,
    warnings: Vec<String>,
}

impl Report {
    fn new() -> Self {
        Report {
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }
    fn err(&mut self, rule: &str, msg: impl Into<String>) {
        self.errors.push(format!("{rule}: {}", msg.into()));
    }
    fn warn(&mut self, rule: &str, msg: impl Into<String>) {
        self.warnings.push(format!("{rule}: {}", msg.into()));
    }
}

fn split_front_matter(text: &str) -> Result<(&str, &str), String> {
    let rest = text
        .strip_prefix("---\n")
        .ok_or("page does not start with a `---` front-matter fence")?;
    let end = rest
        .find("\n---\n")
        .ok_or("front matter has no closing `---` fence")?;
    Ok((&rest[..end], &rest[end + 5..]))
}

fn split_sections(body: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut cur: Option<String> = None;
    let mut buf = String::new();
    let mut in_fence = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && line.starts_with("## ") && !line.starts_with("###") {
            if let Some(h) = cur.take() {
                out.insert(h, std::mem::take(&mut buf));
            }
            cur = Some(line[3..].trim().to_string());
            continue;
        }
        if cur.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(h) = cur {
        out.insert(h, buf);
    }
    out
}

fn headings_in_order(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && line.starts_with("## ") {
            out.push(line[3..].trim().to_string());
        }
    }
    out
}

/// Parse the front-matter subset. `block` names the nested block a key was
/// found in ("" at top level). Unknown keys are collected for P05.
struct FrontMatter {
    scalars: BTreeMap<(String, String), String>,
    related: Vec<String>,
    unknown: Vec<String>,
}

fn parse_front_matter(fm: &str) -> Result<FrontMatter, String> {
    let top_keys = [
        "schema",
        "syscall",
        "nr_x86_64",
        "linux_determinism",
        "target_policy",
        "tracking_status",
        "cross_host_variant",
        "related",
        "discussion",
        "code_facts",
    ];
    let discussion_keys = ["kind", "url"];
    let facts_keys = [
        "as_of",
        "detcore_class",
        "dispatch",
        "refused",
        "refused_only_when_fail_closed",
        "subscribed_run",
        "subscribed_record_replay",
        "tests_naming",
    ];
    let mut out = FrontMatter {
        scalars: BTreeMap::new(),
        related: Vec::new(),
        unknown: Vec::new(),
    };
    let mut block = String::new();
    for raw in fm.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(item) = line.trim_start().strip_prefix("- ") {
            if block != "related" {
                return Err(format!("list item outside `related`: {line:?}"));
            }
            out.related.push(item.trim().to_string());
            continue;
        }
        let indented = line.starts_with("  ");
        let (key, value) = line
            .trim_start()
            .split_once(':')
            .ok_or_else(|| format!("front-matter line is not `key: value`: {line:?}"))?;
        let (key, value) = (key.trim(), value.trim());
        if !indented {
            block = if value.is_empty() {
                key.to_string()
            } else {
                String::new()
            };
            if key == "related" {
                if value == "[]" {
                    block = String::new();
                } else if !value.is_empty() {
                    return Err(format!("`related` must be a list or `[]`, got {value:?}"));
                }
                continue;
            }
            if key == "discussion" || key == "code_facts" {
                if !value.is_empty() {
                    return Err(format!("`{key}` must be a nested block"));
                }
                continue;
            }
            if !top_keys.contains(&key) {
                out.unknown.push(key.to_string());
                continue;
            }
            out.scalars
                .insert((String::new(), key.to_string()), value.to_string());
        } else {
            let known = match block.as_str() {
                "discussion" => discussion_keys.contains(&key),
                "code_facts" => facts_keys.contains(&key),
                _ => false,
            };
            if !known {
                out.unknown.push(format!("{block}.{key}"));
                continue;
            }
            let value = value
                .split(" #")
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"');
            out.scalars
                .insert((block.clone(), key.to_string()), value.to_string());
        }
    }
    Ok(out)
}

fn fm_get<'a>(fm: &'a FrontMatter, block: &str, key: &str) -> Result<&'a str, String> {
    fm.scalars
        .get(&(block.to_string(), key.to_string()))
        .map(|s| s.as_str())
        .ok_or_else(|| format!("front matter is missing `{key}`"))
}

fn parse_bool(s: &str) -> Result<bool, String> {
    match s {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("expected true/false, got {other:?}")),
    }
}

fn parse_page(text: &str, rep: &mut Report) -> Option<Page> {
    let (fm_text, body) = match split_front_matter(text) {
        Ok(v) => v,
        Err(e) => {
            rep.err("P01", e);
            return None;
        }
    };
    let fm = match parse_front_matter(fm_text) {
        Ok(v) => v,
        Err(e) => {
            rep.err("P01", e);
            return None;
        }
    };
    for u in &fm.unknown {
        rep.err("P05", format!("unknown front-matter key `{u}`"));
    }
    let get = |block: &str, key: &str, rep: &mut Report| -> String {
        match fm_get(&fm, block, key) {
            Ok(v) => v.to_string(),
            Err(e) => {
                rep.err("P05", e);
                String::new()
            }
        }
    };
    if get("", "schema", rep) != SCHEMA {
        rep.err("P05", format!("`schema` must be {SCHEMA}"));
    }
    let b = |block: &str, key: &str, rep: &mut Report| -> bool {
        match parse_bool(&get(block, key, rep)) {
            Ok(v) => v,
            Err(e) => {
                rep.err("P05", e);
                false
            }
        }
    };
    let page = Page {
        syscall: get("", "syscall", rep),
        nr_x86_64: get("", "nr_x86_64", rep).parse().unwrap_or(-1),
        linux_determinism: get("", "linux_determinism", rep),
        target_policy: get("", "target_policy", rep),
        tracking_status: get("", "tracking_status", rep),
        cross_host_variant: b("", "cross_host_variant", rep),
        related: fm.related.clone(),
        discussion_kind: get("discussion", "kind", rep),
        discussion_url: get("discussion", "url", rep),
        code_facts: CodeFacts {
            as_of: get("code_facts", "as_of", rep),
            detcore_class: get("code_facts", "detcore_class", rep),
            dispatch: get("code_facts", "dispatch", rep),
            refused: b("code_facts", "refused", rep),
            refused_only_when_fail_closed: b("code_facts", "refused_only_when_fail_closed", rep),
            subscribed_run: b("code_facts", "subscribed_run", rep),
            subscribed_record_replay: b("code_facts", "subscribed_record_replay", rep),
            tests_naming: get("code_facts", "tests_naming", rep).parse().unwrap_or(0),
        },
        sections: split_sections(body),
    };
    Some(page)
}

fn bool_str(v: bool) -> &'static str {
    if v { "true" } else { "false" }
}

fn emit_page(page: &Page) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "---");
    let _ = writeln!(s, "schema: {SCHEMA}");
    let _ = writeln!(s, "syscall: {}", page.syscall);
    let _ = writeln!(s, "nr_x86_64: {}", page.nr_x86_64);
    let _ = writeln!(s, "linux_determinism: {}", page.linux_determinism);
    let _ = writeln!(s, "target_policy: {}", page.target_policy);
    let _ = writeln!(s, "tracking_status: {}", page.tracking_status);
    let _ = writeln!(
        s,
        "cross_host_variant: {}",
        bool_str(page.cross_host_variant)
    );
    if page.related.is_empty() {
        let _ = writeln!(s, "related: []");
    } else {
        let _ = writeln!(s, "related:");
        for u in &page.related {
            let _ = writeln!(s, "  - {u}");
        }
    }
    let _ = writeln!(s, "discussion:");
    let _ = writeln!(s, "  kind: {}", page.discussion_kind);
    let _ = writeln!(s, "  url: {}", page.discussion_url);
    let _ = writeln!(
        s,
        "# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit"
    );
    let _ = writeln!(s, "code_facts:");
    let f = &page.code_facts;
    let _ = writeln!(s, "  as_of: {}", f.as_of);
    let _ = writeln!(s, "  detcore_class: {}", f.detcore_class);
    let _ = writeln!(s, "  dispatch: {:?}", f.dispatch);
    let _ = writeln!(s, "  refused: {}", bool_str(f.refused));
    let _ = writeln!(
        s,
        "  refused_only_when_fail_closed: {}",
        bool_str(f.refused_only_when_fail_closed)
    );
    let _ = writeln!(s, "  subscribed_run: {}", bool_str(f.subscribed_run));
    let _ = writeln!(
        s,
        "  subscribed_record_replay: {}",
        bool_str(f.subscribed_record_replay)
    );
    let _ = writeln!(s, "  tests_naming: {}", f.tests_naming);
    let _ = writeln!(s, "---");
    for h in SECTIONS {
        let _ = writeln!(s, "\n## {h}\n");
        if let Some(body) = page.sections.get(h) {
            s.push_str(body.trim());
            s.push('\n');
        }
    }
    s
}

/// The strategy table column, derived from code facts only. `refused` is
/// split out of `determinized` because the two mean different things to a
/// guest: modelled or emulated behaviour, versus a fixed errno.
fn strategy(f: &CodeFacts) -> &'static str {
    match (f.detcore_class.as_str(), f.refused) {
        ("determinized", true) => "determinized-refused",
        ("determinized", false) => "determinized",
        ("passthrough", _) => "passthrough",
        _ => "unsupported",
    }
}

// ---------------------------------------------------------------------------
// Public-text scrub (P30) and its one-time converter form. Pages live in a
// public repository: no host names, home or scratch paths, internal diff or
// task ids, or internal URLs. `scrub_text` rewrites the forms measured in
// the recovered bodies; `lint` then refuses anything it still recognises,
// so a new form is a visible failure to extend here, not a quiet leak.
// ---------------------------------------------------------------------------

fn scrub_text(t: &str) -> String {
    let t = t
        .replace("this devbig host", "the measurement host")
        .replace("devbig host", "measurement host");
    // Hostname runs anywhere, including inside quoted strace payloads.
    let mut scrubbed = String::with_capacity(t.len());
    let mut rest = t.as_str();
    while let Some(i) = rest.find("devbig") {
        let run_len = rest[i..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
            .map(|c| c.len_utf8())
            .sum::<usize>();
        scrubbed.push_str(&rest[..i]);
        if rest[..i].ends_with('"') {
            scrubbed.push_str("measurement-host");
        } else {
            scrubbed.push_str("the measurement host");
        }
        rest = &rest[i + run_len..];
    }
    scrubbed.push_str(rest);
    let mut out = String::with_capacity(scrubbed.len());
    for word_or_space in scrubbed.split_inclusive(|c: char| c.is_whitespace()) {
        out.push_str(&scrub_token(word_or_space));
    }
    out.replace("on the coordinator host", "on the measurement host")
        .replace("under `/var/tmp`", "in a scratch directory")
        .replace("`/var/tmp/syscall-tracking/", "`a scratch directory (")
}

fn scrub_token(tok: &str) -> String {
    let core = tok.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '-' && c != '/');
    if is_devbig_host(core) {
        return tok.replace(core, "the measurement host");
    }
    if core.starts_with("/var/tmp/") {
        return tok.replace(core, "a scratch directory");
    }
    tok.to_string()
}

fn is_devbig_host(s: &str) -> bool {
    let s = s.strip_suffix("\'s").unwrap_or(s);
    if let Some(rest) = s.strip_prefix("devbig") {
        return rest.chars().take_while(|c| c.is_ascii_digit()).count() >= 3
            && rest[rest.chars().take_while(|c| c.is_ascii_digit()).count()..]
                .chars()
                .all(|c| c == '.' || c.is_alphanumeric());
    }
    false
}

fn scrub_findings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Hostname substrings anywhere, including inside quoted payloads.
    let mut at = 0;
    while let Some(i) = text[at..].find("devbig") {
        let start = at + i;
        let run: String = text[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
            .collect();
        out.push(run.clone());
        at = start + run.len();
    }
    for tok in text.split(|c: char| c.is_whitespace() || c == '`' || c == '(' || c == ')') {
        let core = tok.trim_matches(|c: char| {
            !c.is_alphanumeric() && c != '.' && c != '-' && c != '/' && c != ':'
        });
        if is_devbig_host(core)
            || core.starts_with("/home/")
            || core.starts_with("/data/users/")
            || core.starts_with("/var/tmp/")
            || core.contains(".internalfb.com")
            || core.contains(".atn") && core.contains(".facebook.com")
        {
            out.push(core.to_string());
        }
    }
    // Internal diff and task ids: a capital D or a dh- prefix followed by a
    // long digit/id run, as a standalone token.
    for tok in text.split(|c: char| !(c.is_alphanumeric() || c == '-')) {
        if (tok.starts_with('D') && tok.len() >= 8 && tok[1..].chars().all(|c| c.is_ascii_digit()))
            || (tok.starts_with("dh-") && tok.len() >= 8)
        {
            out.push(tok.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// Lint. Rule ids are P-prefixed (page form); the SCHEMA.md rule index is the
// normative text for each id.
// ---------------------------------------------------------------------------

fn parse_id_list<'a>(prefix: char, lines: impl Iterator<Item = &'a str>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in lines {
        if let Some(rest) = line.trim().strip_prefix("- ") {
            if let Some(id) = rest.split([':', ' ']).next() {
                if id.starts_with(prefix)
                    && id[1..].chars().all(|c| c.is_ascii_digit())
                    && id.len() > 1
                {
                    out.insert(id.to_string());
                }
            }
        }
    }
    out
}

fn lint_page(name_hint: &str, text: &str, root: &Path) -> (Report, Option<Page>) {
    let mut rep = Report::new();
    for hit in scrub_findings(text) {
        rep.err(
            "P30",
            format!("internal or host-specific text must not reach a public page: {hit:?}"),
        );
    }
    // P31 is checked per-section below: the dropped `ptrace_verification`
    // key may still appear inside historical Status entries, nowhere else.
    let page = parse_page(text, &mut rep);
    let Some(page) = page else { return (rep, None) };
    // P02: sections exactly once, in order, nothing before the first.
    if let Ok((_, body)) = split_front_matter(text) {
        let got = headings_in_order(body);
        let want: Vec<String> = SECTIONS.iter().map(|s| s.to_string()).collect();
        if got != want {
            rep.err(
                "P02",
                format!("level-2 headings are {got:?}, want {want:?}"),
            );
        }
        if let Some(first) = body.find("\n## ") {
            if !body[..first].trim().is_empty() && !body.trim_start().starts_with("## ") {
                rep.err("P02", "text before the first level-2 heading");
            }
        }
    }
    // P03: prose sections non-empty.
    for h in [
        "Summary",
        "Linux semantics",
        "Determinism analysis",
        "Hermit design",
        "Done criteria",
    ] {
        if page
            .sections
            .get(h)
            .map(|b| b.trim().is_empty())
            .unwrap_or(true)
        {
            rep.err("P03", format!("section `## {h}` is empty or missing"));
        }
    }
    // P06-P09: judgement and code-fact values.
    if !is_syscall_name(&page.syscall) || page.syscall != name_hint {
        rep.err(
            "P06",
            format!(
                "`syscall: {}` does not match file name {name_hint}",
                page.syscall
            ),
        );
    }
    if !(0..=461).contains(&page.nr_x86_64) {
        rep.err(
            "P07",
            format!(
                "nr_x86_64 {} outside the pinned table range",
                page.nr_x86_64
            ),
        );
    }
    for (rule, v, set) in [
        ("P08", &page.linux_determinism, &LINUX_DETERMINISM[..]),
        ("P08", &page.target_policy, &TARGET_POLICY[..]),
        ("P08", &page.tracking_status, &TRACKING_STATUS[..]),
        ("P09", &page.code_facts.detcore_class, &DETCORE_CLASS[..]),
    ] {
        if !set.contains(&v.as_str()) {
            rep.err(rule, format!("value {v:?} is not one of {set:?}"));
        }
    }
    let f = &page.code_facts;
    if f.as_of.len() != 40
        || !f
            .as_of
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        rep.err("P09", "`code_facts.as_of` must be a 40-hex hermit SHA");
    }
    let dispatch_ok = [
        "typed",
        "sysno-guard",
        "passthrough",
        "unsupported",
        "fallback",
    ]
    .contains(&f.dispatch.as_str())
        || f.dispatch
            .strip_prefix("pred:is_")
            .map(is_syscall_name)
            .unwrap_or(false);
    if !dispatch_ok {
        rep.err("P09", format!("dispatch {:?} is not typed | sysno-guard | pred:is_<fn> | passthrough | unsupported | fallback", f.dispatch));
    }
    for u in &page.related {
        if !u.starts_with("https://github.com/") {
            rep.err(
                "P08",
                format!("`related` holds full GitHub URLs only: {u:?}"),
            );
        }
    }
    for (h, b) in &page.sections {
        if h != "Status" && b.contains("ptrace_verification") {
            rep.err(
                "P31",
                format!("`ptrace_verification` was dropped from page form (in `## {h}`)"),
            );
        }
    }
    lint_flags_tests(&page, &mut rep);
    lint_status(&page, root, &mut rep);
    lint_done(&page, &mut rep);
    lint_discussion(&page, &mut rep);
    // W01: stale issue-era denominators. W02: nondet passthrough surprise.
    let prose: String = [
        "Summary",
        "Linux semantics",
        "Determinism analysis",
        "Hermit design",
    ]
    .iter()
    .filter_map(|h| page.sections.get(*h))
    .cloned()
    .collect();
    for needle in [
        "UNCLASSIFIED (TEMPORARY PASS-THRU)",
        "temporary pass-through",
    ] {
        if prose.contains(needle) {
            rep.warn("W01", format!("stale text mentions {needle:?}"));
        }
    }
    if page.linux_determinism == "nondeterministic"
        && page.code_facts.detcore_class == "passthrough"
        && page.tracking_status != "blocked"
    {
        rep.warn(
            "W02",
            "nondeterministic under Linux but passed through while not blocked",
        );
    }
    (rep, Some(page))
}

/// Does the text cite a `path:line` (a source path with an extension, a
/// colon, and a line number)? Used for U items and design citations.
fn has_path_line(text: &str) -> bool {
    for tok in text.split(|c: char| c.is_whitespace() || c == '`' || c == '(' || c == ')') {
        for ext in [".rs:", ".c:", ".h:", ".yaml:", ".toml:", ".md:"] {
            if let Some(i) = tok.find(ext) {
                let after = &tok[i + ext.len()..];
                if after.chars().take_while(|c| c.is_ascii_digit()).count() > 0 {
                    return true;
                }
            }
        }
    }
    false
}

fn subsection<'a>(body: &'a str, heading: &str) -> Option<&'a str> {
    let marker = format!("### {heading}");
    let start = body.find(&marker)? + marker.len();
    let rest = &body[start..];
    let end = rest.find("\n### ").unwrap_or(rest.len());
    Some(&rest[..end])
}

fn lint_flags_tests(page: &Page, rep: &mut Report) {
    let linux = page
        .sections
        .get("Linux semantics")
        .cloned()
        .unwrap_or_default();
    // P10/P29: exactly one flags inventory; cross-host subsection iff flag.
    let flags_count = linux.matches("### Flags and options").count();
    if flags_count != 1 {
        rep.err("P10", format!("`## Linux semantics` must hold exactly one `### Flags and options`, found {flags_count}"));
    }
    let flags_body = subsection(&linux, "Flags and options").unwrap_or("");
    let f_ids = parse_id_list('F', flags_body.lines());
    let sentinel = flags_body.trim() == "_None: the syscall takes no flags or options._";
    if f_ids.is_empty() && !sentinel {
        rep.err(
            "P10",
            "flags inventory is empty and is not the `_None_` sentinel",
        );
    }
    for line in flags_body.lines() {
        let t = line.trim();
        if !t.is_empty() && !t.starts_with("- ") && !sentinel && !line.starts_with("  ") {
            rep.err("P10", format!("stray line in flags inventory: {t:?}"));
        }
    }
    let det = page
        .sections
        .get("Determinism analysis")
        .cloned()
        .unwrap_or_default();
    let xh = subsection(&det, "Cross-host variation");
    let xh_items = xh
        .map(|b| b.lines().filter(|l| l.trim().starts_with("- ")).count())
        .unwrap_or(0);
    if page.cross_host_variant != (xh.is_some() && xh_items > 0) {
        rep.err("P29", "`cross_host_variant` is true iff one `### Cross-host variation` lists the host properties");
    }
    // P11: T/U items reference defined F ids; ids unique.
    let tests = page
        .sections
        .get("Recommended tests")
        .cloned()
        .unwrap_or_default();
    if tests.trim() == "_None proposed yet._" {
        if page.tracking_status != "todo" {
            rep.err(
                "P11",
                "`_None proposed yet._` is only allowed while `tracking_status: todo`",
            );
        }
        return;
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for line in tests.lines() {
        // Indented `- ` lines are sub-cases of the item above them.
        let Some(rest) = line.strip_prefix("- ") else {
            continue;
        };
        let Some(id) = rest.split([' ', ':']).next() else {
            continue;
        };
        if !(id.starts_with(['T', 'U'])
            && id.len() > 1
            && id[1..].chars().all(|c| c.is_ascii_digit()))
        {
            rep.err(
                "P11",
                format!("recommended-test item does not start with a T/U id: {rest:?}"),
            );
            continue;
        }
        if !seen.insert(id.to_string()) {
            rep.err("P11", format!("repeated test id {id}"));
        }
        let Some(open) = rest.find("(covers ") else {
            rep.err("P11", format!("{id} has no `(covers ...)` list"));
            continue;
        };
        let Some(close) = rest[open..].find(')') else {
            continue;
        };
        let covers = &rest[open + 8..open + close];
        if id.starts_with('U') {
            if !covers.contains("host-config") && !covers.contains("host-transient") {
                rep.err(
                    "P11",
                    format!("{id} U item must be host-config or host-transient"),
                );
            }
            if covers.contains("host-config") && !page.cross_host_variant {
                rep.err(
                    "P32",
                    format!("{id} host-config U item requires `cross_host_variant: true`"),
                );
            }
            if !has_path_line(rest) {
                rep.err(
                    "P11",
                    format!("{id} U item must cite the `path:line` that forwards the error"),
                );
            }
        }
        for c in covers.split([',', ';']) {
            let c = c.trim();
            if c.starts_with('F')
                && c.len() > 1
                && c[1..].chars().all(|x| x.is_ascii_digit())
                && !f_ids.contains(c)
            {
                rep.err("P11", format!("{id} covers undefined flag {c}"));
            }
        }
    }
    if seen.is_empty() {
        rep.err("P11", "`## Recommended tests` has no T or U items");
    }
}

fn status_entries(page: &Page) -> Vec<String> {
    page.sections
        .get("Status")
        .map(|b| {
            b.lines()
                .filter(|l| l.trim_start().starts_with("- "))
                .map(|l| l.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Coverage tokens a Status log actually asserts: `T<n> covered by <path>`
/// inside an entry whose text starts `covered:` or `done:`, minus any T id a
/// later `uncovered:` entry withdraws.
fn coverage_tokens(page: &Page) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for e in status_entries(page) {
        let text = e.rsplit(" · ").next().unwrap_or("");
        if text.starts_with("uncovered:") {
            for tok in text.split(|c: char| !(c.is_alphanumeric())) {
                if tok.starts_with('T')
                    && tok.len() > 1
                    && tok[1..].chars().all(|c| c.is_ascii_digit())
                {
                    out.retain(|(t, _)| t != tok);
                }
            }
            continue;
        }
        if !(text.starts_with("covered:") || text.starts_with("done:")) {
            continue;
        }
        let mut rest = text;
        while let Some(i) = rest.find(" covered by ") {
            let before = &rest[..i];
            let Some(tid) = before.rsplit(|c: char| !(c.is_alphanumeric())).next() else {
                break;
            };
            let path = rest[i + 12..]
                .split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == ')')
                .next()
                .unwrap_or("");
            if tid.starts_with('T') && tid.len() > 1 && !path.is_empty() {
                out.push((tid.to_string(), path.trim_matches('`').to_string()));
            }
            rest = &rest[i + 12..];
        }
    }
    out
}

fn lint_status(page: &Page, root: &Path, rep: &mut Report) {
    let entries = status_entries(page);
    if entries.is_empty() {
        rep.err("P12", "`## Status` needs at least one dated entry");
    }
    let mut last = "";
    for e in &entries {
        let parts: Vec<&str> = e.trim_start_matches("- ").split(" · ").collect();
        if parts.len() < 4 {
            rep.err(
                "P12",
                format!("Status entry is not `- date · hermit <sha> · agent · text`: {e:?}"),
            );
            continue;
        }
        let date = parts[0];
        let ok_date = date.len() == 10
            && date.as_bytes().get(4) == Some(&b'-')
            && date.as_bytes().get(7) == Some(&b'-')
            && date.chars().filter(|c| c.is_ascii_digit()).count() == 8;
        if !ok_date {
            rep.err("P12", format!("Status entry date is not YYYY-MM-DD: {e:?}"));
        }
        if date < last {
            rep.err("P13", format!("Status dates decrease at {e:?}"));
        }
        last = date;
        let sha_ok = parts[1]
            .strip_prefix("hermit ")
            .map(|s| {
                (12..=40).contains(&s.len())
                    && s.chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            })
            .unwrap_or(false);
        if !sha_ok {
            rep.err(
                "P12",
                format!("Status entry does not name `hermit <12-40 hex>`: {e:?}"),
            );
        }
    }
    // P33 (existence half of the old C08): every asserted coverage path
    // exists and the T id is defined. The strict-container half is the
    // recorded follow-up in the file header and SCHEMA.md.
    let tests = page
        .sections
        .get("Recommended tests")
        .cloned()
        .unwrap_or_default();
    let t_ids = parse_id_list('T', tests.lines());
    for (tid, path) in coverage_tokens(page) {
        if !t_ids.contains(&tid) {
            rep.err("P33", format!("coverage token names undefined {tid}"));
        }
        if !root.join(&path).exists() {
            rep.err(
                "P33",
                format!("coverage path {path} for {tid} does not exist in the tree"),
            );
        }
    }
}

fn lint_done(page: &Page, rep: &mut Report) {
    let entries = status_entries(page);
    let last_text = entries
        .last()
        .map(|e| e.rsplit(" · ").next().unwrap_or("").to_string())
        .unwrap_or_default();
    if page.tracking_status == "wontfix" && !last_text.starts_with("wontfix:") {
        rep.err(
            "P18",
            "`wontfix` needs a last Status entry starting `wontfix:`",
        );
    }
    if page.tracking_status != "done" {
        return;
    }
    let f = &page.code_facts;
    let realises = match page.target_policy.as_str() {
        "refuse" => f.refused,
        "model" | "emulate" => f.detcore_class == "determinized" && !f.refused,
        "passthrough" => f.detcore_class == "passthrough",
        _ => false,
    };
    if !realises {
        rep.err(
            "P14",
            format!(
                "`done` needs code facts to realise target_policy {}",
                page.target_policy
            ),
        );
    }
    if !last_text.starts_with("done:") {
        rep.err("P16", "`done` needs a last Status entry starting `done:`");
    }
    let linux = page
        .sections
        .get("Linux semantics")
        .cloned()
        .unwrap_or_default();
    let f_ids = parse_id_list(
        'F',
        subsection(&linux, "Flags and options")
            .unwrap_or("")
            .lines(),
    );
    let tests = page
        .sections
        .get("Recommended tests")
        .cloned()
        .unwrap_or_default();
    let covered_t: BTreeSet<String> = coverage_tokens(page).into_iter().map(|(t, _)| t).collect();
    let mut covered_f: BTreeSet<String> = BTreeSet::new();
    for line in tests.lines() {
        let Some(rest) = line.trim().strip_prefix("- ") else {
            continue;
        };
        let Some(id) = rest.split([' ', ':']).next() else {
            continue;
        };
        let Some(open) = rest.find("(covers ") else {
            continue;
        };
        let Some(close) = rest[open..].find(')') else {
            continue;
        };
        let covers = &rest[open + 8..open + close];
        let counts =
            covered_t.contains(id) || (id.starts_with('U') && covers.contains("host-config"));
        if counts {
            for c in covers.split([',', ';']) {
                let c = c.trim();
                if c.starts_with('F') {
                    covered_f.insert(c.to_string());
                }
            }
        }
    }
    if covered_t.is_empty() {
        rep.err("P17", "`done` needs at least one recorded coverage token");
    }
    for fid in &f_ids {
        if !covered_f.contains(fid) {
            rep.err(
                "P17",
                format!("`done` but flag {fid} has no covered test or host-config U item"),
            );
        }
    }
}

fn lint_discussion(page: &Page, rep: &mut Report) {
    match page.discussion_kind.as_str() {
        "existing" => {
            if !page
                .discussion_url
                .starts_with("https://github.com/facebookexperimental/hermit/issues/")
            {
                rep.err(
                    "P34",
                    "an `existing` discussion must link a facebookexperimental/hermit issue",
                );
            }
        }
        "new" => {
            if !page
                .discussion_url
                .starts_with("https://github.com/facebookexperimental/hermit/issues/new?")
            {
                rep.err("P34", "a `new` discussion must be a prefilled facebookexperimental/hermit new-issue URL");
            }
        }
        other => rep.err(
            "P34",
            format!("discussion kind must be existing or new, got {other:?}"),
        ),
    }
    let sec = page.sections.get("Discussion").cloned().unwrap_or_default();
    if !sec.contains(&page.discussion_url) {
        rep.err(
            "P34",
            "`## Discussion` must link the front-matter discussion URL",
        );
    }
}

// ---------------------------------------------------------------------------
// Dispatch parse (P27). Adapted from the dev-hermit C4 parse and extended
// for today's dispatcher: wildcard pre-arms, #[cfg] attributes on arms and
// inside predicate bodies, and match-form predicates. Anything else shape
// is `parser-failed`, never a pass.
// ---------------------------------------------------------------------------

fn mask_rust(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for x in out.iter_mut().take(to).skip(from) {
            if *x != b'\n' {
                *x = b' ';
            }
        }
    };
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            let end = src[i..].find('\n').map(|k| i + k).unwrap_or(b.len());
            blank(&mut out, i, end);
            i = end;
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            let (mut depth, mut j) = (1, i + 2);
            while j < b.len() && depth > 0 {
                if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            blank(&mut out, i, j);
            i = j;
        } else if b[i] == b'"' {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            blank(&mut out, i + 1, j.min(b.len()));
            i = j + 1;
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

fn matching(m: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (k, &c) in m.iter().enumerate().skip(open) {
        match c {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone)]
struct Arm {
    pattern: String,
    body: String,
    start: usize,
    /// Attribute texts (without `#[` / `]`) that preceded the pattern.
    attrs: Vec<String>,
}

/// Split match arms on masked source `m`; attribute text is read from the
/// original source at the same offsets (masking blanks string contents, and
/// the arch in `#[cfg(target_arch = "...")]` is a string).
fn split_arms(m: &str, orig: &str, from: usize, to: usize) -> Result<Vec<Arm>, String> {
    let b = m.as_bytes();
    let mut arms = Vec::new();
    let mut i = from;
    loop {
        while i < to && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= to {
            break;
        }
        // Capture leading attributes (#[cfg(...)]) before an arm pattern;
        // arch-gated arms are handled by the caller.
        let mut start = i;
        let mut attrs = Vec::new();
        while start + 1 < to && b[start] == b'#' && b[start + 1] == b'[' {
            let close = matching(b, start + 1).ok_or("unbalanced arm attribute")?;
            attrs.push(one_line(&orig[start + 2..close]));
            start = close + 1;
            while start < to && b[start].is_ascii_whitespace() {
                start += 1;
            }
        }
        let mut depth = 0i32;
        let mut arrow = None;
        let mut k = start;
        while k + 1 < to {
            match b[k] {
                b'{' | b'(' | b'[' => depth += 1,
                b'}' | b')' | b']' => depth -= 1,
                b'=' if depth == 0 && b[k + 1] == b'>' => {
                    arrow = Some(k);
                    break;
                }
                _ => {}
            }
            k += 1;
        }
        let Some(a) = arrow else {
            return Err(format!("no `=>` after byte {start}"));
        };
        let pattern = one_line(&m[start..a]);
        let mut j = a + 2;
        while j < to && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let body_start = j;
        let body_end;
        if j < to && b[j] == b'{' {
            let close = matching(b, j).ok_or("unbalanced arm body")?;
            body_end = close + 1;
            j = close + 1;
            while j < to && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < to && b[j] == b',' {
                j += 1;
            }
        } else {
            let mut depth = 0i32;
            while j < to {
                match b[j] {
                    b'{' | b'(' | b'[' => depth += 1,
                    b'}' | b')' | b']' => depth -= 1,
                    b',' if depth == 0 => break,
                    _ => {}
                }
                j += 1;
            }
            body_end = j;
            if j < to {
                j += 1;
            }
        }
        arms.push(Arm {
            pattern,
            body: m[body_start..body_end].to_string(),
            start,
            attrs,
        });
        i = j;
    }
    Ok(arms)
}

fn split_top(s: &str, sep: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut last, mut i) = (0i32, 0usize, 0usize);
    while i < b.len() {
        match b[i] {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => depth -= 1,
            _ => {}
        }
        if depth == 0 && s[i..].starts_with(sep) {
            let pipe_pair = sep == "|" && (s[i..].starts_with("||") || (i > 0 && b[i - 1] == b'|'));
            if !pipe_pair {
                out.push(s[last..i].trim().to_string());
                i += sep.len();
                last = i;
                continue;
            }
        }
        i += 1;
    }
    out.push(s[last..].trim().to_string());
    out
}

type PredDef = Result<(BTreeSet<String>, BTreeSet<String>), String>;

fn ident_after(m: &str, at: usize) -> Option<(String, usize)> {
    let rest = &m[at..];
    let len = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .map(|c| c.len_utf8())
        .sum::<usize>();
    if len == 0 {
        None
    } else {
        Some((rest[..len].to_string(), at + len))
    }
}

/// Strip #[...] attributes from masked source (cfg gates on arms). Members
/// named only under a non-x86_64 cfg are not in the pinned set and drop out
/// when membership is intersected with it; a member gated to x86_64 stays.
fn strip_attrs(m: &str) -> String {
    let b = m.as_bytes();
    let mut out = String::with_capacity(m.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' && b.get(i + 1) == Some(&b'[') {
            match matching(b, i + 1) {
                Some(close) => {
                    out.push(' ');
                    i = close + 1;
                }
                None => break,
            }
        } else {
            let ch = m[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn parse_predicates(src: &str, file: &str) -> Vec<(String, Option<PredDef>)> {
    let m = strip_attrs(&mask_rust(src));
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(off) = m[at..].find("fn is_") {
        let start = at + off + 3;
        let Some((name, after)) = ident_after(&m, start) else {
            break;
        };
        at = after;
        let Some(paren) = m[after..].find('(') else {
            continue;
        };
        let sig_ok = m[after..after + paren].trim().is_empty()
            && m[after + paren..]
                .trim_start()
                .starts_with("(sysno: Sysno) -> bool {");
        if !sig_ok {
            continue;
        }
        let open = after + paren;
        let Some(sig_end) = m[open..].find('{') else {
            continue;
        };
        let open = open + sig_end;
        let Some(close) = matching(m.as_bytes(), open) else {
            out.push((
                name.clone(),
                Some(Err(format!("{file}: unbalanced body for {name}"))),
            ));
            continue;
        };
        let body = &m[open + 1..close];
        if body.trim() == format!("crate::syscall_classification::{name}(sysno)")
            || body.trim() == format!("syscall_classification::{name}(sysno)")
        {
            out.push((name, None));
            continue;
        }
        let mut members = BTreeSet::new();
        let mut subs = BTreeSet::new();
        let mut rest = body.to_string();
        while let Some(i) = rest.find("Sysno::") {
            match ident_after(&rest, i + 7) {
                Some((id, end)) => {
                    members.insert(id);
                    rest.replace_range(i..end, " ");
                }
                None => break,
            }
        }
        while let Some(i) = rest.find("is_") {
            match ident_after(&rest, i) {
                Some((id, end)) => {
                    if rest[end..].trim_start().starts_with("(sysno)") {
                        subs.insert(id);
                    }
                    rest.replace_range(i..end, " ");
                }
                None => break,
            }
        }
        for word in ["matches!", "match", "sysno", "true", "false", "_"] {
            rest = rest.replace(word, " ");
        }
        let leftover: String = rest
            .chars()
            .filter(|ch| !ch.is_whitespace() && !"()|,{}=>".contains(*ch))
            .collect();
        if !leftover.is_empty() || body.contains("&&") {
            out.push((name.clone(), Some(Err(format!(
                "{file}: predicate {name} has a body the parser does not understand: {leftover:?}"
            )))));
            continue;
        }
        out.push((name, Some(Ok((members, subs)))));
    }
    out
}

fn merge_predicates(files: &[(&str, &str)]) -> BTreeMap<String, PredDef> {
    let mut all: BTreeMap<String, Vec<PredDef>> = BTreeMap::new();
    let mut delegates: BTreeSet<String> = BTreeSet::new();
    for (file, src) in files {
        for (name, def) in parse_predicates(src, file) {
            match def {
                Some(d) => all.entry(name).or_default().push(d),
                None => {
                    delegates.insert(name);
                }
            }
        }
    }
    let mut out: BTreeMap<String, PredDef> = BTreeMap::new();
    for name in delegates.iter().filter(|n| !all.contains_key(*n)) {
        out.insert(
            name.clone(),
            Err(format!(
                "predicate {name} only delegates; definition not found"
            )),
        );
    }
    for (name, mut defs) in all {
        let d = if defs.len() == 1 {
            defs.remove(0)
        } else {
            Err(format!("predicate {name} is defined {} times", defs.len()))
        };
        out.insert(name, d);
    }
    out
}

fn expand_pred(
    name: &str,
    preds: &BTreeMap<String, PredDef>,
    seen: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> BTreeSet<String> {
    let Some(def) = preds.get(name) else {
        errors.push(format!(
            "predicate {name} is used but its definition was not found"
        ));
        return BTreeSet::new();
    };
    let (members, subs) = match def {
        Ok(d) => d,
        Err(why) => {
            errors.push(format!(
                "predicate {name} is used by the dispatch but was not understood: {why}"
            ));
            return BTreeSet::new();
        }
    };
    if seen.iter().any(|s| s == name) {
        errors.push(format!("predicate cycle through {name}"));
        return BTreeSet::new();
    }
    seen.push(name.to_string());
    let mut out = members.clone();
    for s in subs {
        out.extend(expand_pred(s, preds, seen, errors));
    }
    seen.pop();
    out
}

/// Pages describe x86_64. An arm gated to aarch64 does not run there and
/// assigns nothing; an arm gated to x86_64 (or not-aarch64) does. Any other
/// cfg shape is not understood.
fn arm_arch(attrs: &[String]) -> Result<bool, String> {
    let mut applies = true;
    for a in attrs {
        let a = a.replace(' ', "");
        match a.as_str() {
            "cfg(target_arch=\"aarch64\")" => applies = false,
            "cfg(target_arch=\"x86_64\")" | "cfg(not(target_arch=\"aarch64\"))" => {}
            other => return Err(format!("arm attribute #[{other}] is not understood")),
        }
    }
    Ok(applies)
}

fn camel(name: &str) -> String {
    name.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

struct DispatchReport {
    status: String,
    errors: Vec<String>,
    dispatch: BTreeMap<String, String>,
    missing_arm: Vec<String>,
}

/// Assign each pinned syscall the dispatch arm that handles it. `classes`
/// maps name -> detcore_class. Determinized syscalls with no explicit arm
/// get `fallback`; any shape the parser does not understand makes the whole
/// report `parser-failed`.
fn parse_dispatch(
    lib: &str,
    preds_src: &[(&str, &str)],
    classes: &BTreeMap<String, String>,
) -> DispatchReport {
    let mut errors: Vec<String> = Vec::new();
    let fail = |errors: Vec<String>, classes: &BTreeMap<String, String>| DispatchReport {
        status: "parser-failed".into(),
        errors,
        dispatch: classes
            .keys()
            .map(|k| (k.clone(), "parser-failed".into()))
            .collect(),
        missing_arm: Vec::new(),
    };
    let m = mask_rust(lib);
    let needle = "match classify_syscall(call.number()) {";
    let hits: Vec<usize> = m.match_indices(needle).map(|(i, _)| i).collect();
    if hits.len() != 1 {
        errors.push(format!(
            "expected exactly one `{needle}` in detcore/src/lib.rs, found {}",
            hits.len()
        ));
        return fail(errors, classes);
    }
    let open = hits[0] + needle.len() - 1;
    let Some(close) = matching(m.as_bytes(), open) else {
        errors.push("unbalanced classify_syscall match".into());
        return fail(errors, classes);
    };
    let arms = match split_arms(&m, lib, open + 1, close) {
        Ok(a) => a,
        Err(e) => {
            errors.push(format!("cannot split classify_syscall arms: {e}"));
            return fail(errors, classes);
        }
    };
    let preds = merge_predicates(preds_src);
    let mut dispatch: BTreeMap<String, String> = BTreeMap::new();
    let mut typed_arm: Option<Arm> = None;
    for a in &arms {
        match arm_arch(&a.attrs) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(e) => {
                errors.push(e);
                continue;
            }
        }
        let pat = a.pattern.as_str();
        // Wildcard pre-arms intercept conditional subsets and fall through
        // to the classifying arms when their guard is false; they assign no
        // syscall. An unguarded wildcard would swallow the match: refuse.
        if pat == "_" || pat.starts_with("_ if ") {
            if pat == "_" {
                errors.push(format!("unguarded wildcard outer arm at byte {}", a.start));
            }
            continue;
        }
        let Some(cls) = ["Determinized", "PassThrough", "Unsupported"]
            .iter()
            .find(|c| pat.starts_with(&format!("SyscallClassification::{c}")))
        else {
            errors.push(format!(
                "unrecognised outer arm pattern {pat:?} at byte {}",
                a.start
            ));
            continue;
        };
        let rest = pat["SyscallClassification::".len() + cls.len()..].trim();
        if rest.is_empty() {
            if *cls == "Determinized" {
                typed_arm = Some(a.clone());
            }
            continue;
        }
        let Some(guard) = rest.strip_prefix("if ") else {
            errors.push(format!("outer arm pattern {pat:?} has trailing {rest:?}"));
            continue;
        };
        // A guard with an extra `&&` condition (today only the PassThrough
        // brk launcher arm) assigns nothing: the syscall still needs its
        // unconditional arm below. On a Determinized arm that shape is not
        // understood and fails the parse rather than reading as handled.
        if guard.contains("&&") {
            if *cls == "Determinized" {
                errors.push(format!(
                    "Determinized outer guard with && is not understood: {guard:?}"
                ));
            }
            continue;
        }
        if guard.starts_with("matches!(") {
            let mut rest = guard.to_string();
            while let Some(i) = rest.find("Sysno::") {
                match ident_after(&rest, i + 7) {
                    Some((name, end)) => {
                        if classes.contains_key(&name) {
                            dispatch.insert(name, "sysno-guard".into());
                        }
                        rest.replace_range(i..end, " ");
                    }
                    None => break,
                }
            }
            continue;
        }
        for term in split_top(guard, "||") {
            if let Some(name) = term.strip_prefix("call.number() == Sysno::") {
                if is_syscall_name(name) {
                    dispatch.insert(name.to_string(), "sysno-guard".into());
                } else {
                    errors.push(format!("bad sysno guard term {term:?}"));
                }
            } else if term.starts_with("is_") && term.ends_with("(call.number())") {
                let pred = &term[..term.len() - "(call.number())".len()];
                let pred_ok = pred.len() > 3
                    && pred[3..]
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                if pred_ok {
                    for member in expand_pred(pred, &preds, &mut Vec::new(), &mut errors) {
                        if classes.contains_key(&member) {
                            dispatch.insert(member, format!("pred:{pred}"));
                        }
                    }
                } else {
                    errors.push(format!("bad predicate guard term {term:?}"));
                }
            } else {
                errors.push(format!(
                    "guard term {term:?} is neither a Sysno equality nor a predicate call"
                ));
            }
        }
    }
    // Typed inner match.
    let variants: BTreeMap<String, String> =
        classes.keys().map(|n| (camel(n), n.clone())).collect();
    let mut has_fallback = false;
    match &typed_arm {
        None => errors
            .push("no unguarded `SyscallClassification::Determinized => match call` arm".into()),
        Some(o) => {
            let body = o.body.trim_start();
            if !body.starts_with("match call {") {
                errors.push("the unguarded Determinized arm is not `match call { ... }`".into());
            } else {
                // The `{` of `match call {` in masked source, at/after the arm.
                let rel = m[o.start..].find("match call {").unwrap_or(0);
                let iopen = o.start + rel + "match call ".len();
                if let Some(ic) = matching(m.as_bytes(), iopen) {
                    match split_arms(&m, lib, iopen + 1, ic) {
                        Ok(inner) => {
                            for a in &inner {
                                if let Err(e) = arm_arch(&a.attrs) {
                                    errors.push(e);
                                }
                            }
                            let inner: Vec<&Arm> = inner
                                .iter()
                                .filter(|a| matches!(arm_arch(&a.attrs), Ok(true)))
                                .collect();
                            let n = inner.len();
                            for (k, a) in inner.iter().enumerate() {
                                let parts = split_top(&a.pattern, " if ");
                                let alts = split_top(&parts[0], "|");
                                if alts.len() == 1
                                    && (alts[0] == "_"
                                        || (is_syscall_name(&alts[0])
                                            && !alts[0].starts_with("Syscall")))
                                {
                                    if k + 1 != n || parts.len() > 1 {
                                        errors.push(format!(
                                            "catch-all arm {:?} is not the last unguarded arm",
                                            a.pattern
                                        ));
                                    }
                                    has_fallback = true;
                                    continue;
                                }
                                for alt in alts {
                                    let Some(vrest) = alt.trim().strip_prefix("Syscall::") else {
                                        errors.push(format!("typed arm alternative {alt:?} does not start with Syscall::"));
                                        continue;
                                    };
                                    let Some((variant, _)) = ident_after(vrest, 0) else {
                                        continue;
                                    };
                                    match variants.get(&variant) {
                                        Some(name) => {
                                            dispatch.entry(name.clone()).or_insert("typed".into());
                                        }
                                        None => {
                                            if variant != "Other" {
                                                errors.push(format!("typed variant Syscall::{variant} maps to no pinned syscall"));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => errors.push(format!("cannot split typed arms: {e}")),
                    }
                } else {
                    errors.push("unbalanced typed `match call`".into());
                }
            }
        }
    }
    if !has_fallback {
        errors.push("typed `match call` has no catch-all fallback arm".into());
    }
    let mut missing = Vec::new();
    for (name, cls) in classes {
        let d = match cls.as_str() {
            "passthrough" => dispatch.get(name).cloned().unwrap_or("passthrough".into()),
            "unsupported" => "unsupported".into(),
            _ => match dispatch.get(name) {
                Some(d) => d.clone(),
                None => {
                    missing.push(name.clone());
                    "fallback".into()
                }
            },
        };
        dispatch.insert(name.clone(), d);
    }
    if !errors.is_empty() {
        return fail(errors, classes);
    }
    DispatchReport {
        status: "ok".into(),
        errors,
        dispatch,
        missing_arm: missing,
    }
}

// ---------------------------------------------------------------------------
// Facts: the TSV the exporter prints, the dispatch parse, and the C9 count
// of test files naming a syscall. Facts are bound to an exact HEAD: the
// exporter binary carries the SHA it was built against and refuses others,
// and this tool refuses to build it from a dirty tree.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FactRow {
    nr: i32,
    detcore_class: String,
    refused: bool,
    refused_only_when_fail_closed: bool,
    subscribed_run: bool,
    subscribed_record_replay: bool,
}

fn parse_facts_tsv(text: &str) -> Result<(String, BTreeMap<String, FactRow>), String> {
    let mut sha = String::new();
    let mut rows = BTreeMap::new();
    for line in text.lines() {
        if let Some(s) = line.strip_prefix("# hermit ") {
            sha = s.trim().to_string();
            continue;
        }
        if line.starts_with("name\t") || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 7 {
            return Err(format!("facts line does not have 7 columns: {line:?}"));
        }
        let bit = |s: &str| s == "1";
        rows.insert(
            f[0].to_string(),
            FactRow {
                nr: f[1]
                    .parse()
                    .map_err(|_| format!("bad nr in facts line {line:?}"))?,
                detcore_class: f[2].to_string(),
                refused: bit(f[3]),
                refused_only_when_fail_closed: bit(f[4]),
                subscribed_run: bit(f[5]),
                subscribed_record_replay: bit(f[6]),
            },
        );
    }
    if sha.is_empty() {
        return Err("facts TSV does not name its hermit SHA".into());
    }
    Ok((sha, rows))
}

fn run_cmd(cmd: &mut Command) -> Result<String, String> {
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {cmd:?}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(600)
                .collect::<String>()
        ))
    }
}

fn git_head(root: &Path) -> String {
    run_cmd(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"]),
    )
    .map(|s| s.trim().to_string())
    .unwrap_or_else(|e| fail(&e))
}

fn export_facts(root: &Path) -> Result<(String, BTreeMap<String, FactRow>), String> {
    let sha = git_head(root);
    let dirty = run_cmd(Command::new("git").arg("-C").arg(root).args([
        "status",
        "--porcelain",
        "--",
        "detcore",
        "detcore-model",
        "reverie",
        "Cargo.lock",
    ]))?;
    if !dirty.trim().is_empty() {
        return Err(format!(
            "refusing to export facts from a dirty tree (detcore, detcore-model, reverie, Cargo.lock); commit or stash first:\n{dirty}"
        ));
    }
    let template = read(&root.join("scripts/syscall-pages-facts.rs"));
    let build = root.join("ignored/syscall-pages/facts-build").join(&sha);
    fs::create_dir_all(build.join("src")).map_err(|e| e.to_string())?;
    let root_s = root.to_string_lossy().into_owned();
    let main_rs = template
        .replace("@HERMIT_SHA@", &sha)
        .lines()
        .filter(|l| !l.starts_with("//!"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(build.join("src/main.rs"), main_rs).map_err(|e| e.to_string())?;
    let reverie_ok = root.join("reverie/reverie/Cargo.toml").exists();
    let reverie_rev = run_cmd(
        Command::new("git")
            .arg("-C")
            .arg(root.join("reverie"))
            .args(["rev-parse", "HEAD"]),
    )
    .map(|s| s.trim().to_string())
    .unwrap_or_default();
    let detcore_manifest = read(&root.join("detcore/Cargo.toml"));
    if !reverie_ok || !detcore_manifest.contains(&reverie_rev) || reverie_rev.is_empty() {
        return Err(
            "the checkout's reverie/ tree does not match detcore's pinned reverie rev".into(),
        );
    }
    let manifest = format!(
        "[package]\nname = \"syscall-pages-facts-build\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[dependencies]\ndetcore = {{ package = \"hermit-detcore\", path = \"{root_s}/detcore\" }}\nreverie = {{ package = \"reverie-core\", path = \"{root_s}/reverie/reverie\" }}\n\n[patch.\"https://github.com/rrnewton/reverie.git\"]\nreverie-core = {{ path = \"{root_s}/reverie/reverie\" }}\nreverie-syscalls = {{ path = \"{root_s}/reverie/reverie-syscalls\" }}\nreverie-process = {{ path = \"{root_s}/reverie/reverie-process\" }}\nreverie-memory = {{ path = \"{root_s}/reverie/reverie-memory\" }}\n"
    );
    fs::write(build.join("Cargo.toml"), manifest).map_err(|e| e.to_string())?;
    fs::copy(root.join("Cargo.lock"), build.join("Cargo.lock")).map_err(|e| e.to_string())?;
    // Honour an external CARGO_TARGET_DIR (freeze windows keep build output
    // out of the tree); default beside the generated project under ignored/.
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(|t| PathBuf::from(t).join(format!("syscall-pages-facts-{sha}")))
        .unwrap_or_else(|| build.join("target"));
    run_cmd(
        Command::new("cargo")
            .args(["build", "--offline", "--manifest-path"])
            .arg(build.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", &target),
    )?;
    let bin = target.join("debug/syscall-pages-facts-build");
    let tsv = run_cmd(Command::new(&bin).args(["--expect-sha", &sha]))?;
    let facts_cache = root
        .join("ignored/syscall-pages/facts")
        .join(format!("{sha}.tsv"));
    if let Some(parent) = facts_cache.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&facts_cache, &tsv).map_err(|e| e.to_string())?;
    parse_facts_tsv(&tsv)
}

/// C9: for each syscall, how many test files name it via SYS_, __NR_ or
/// Sysno::. Advisory only; it under-counts libc wrapper use by design.
fn tests_naming(root: &Path, names: &BTreeSet<String>) -> BTreeMap<String, u32> {
    let mut per_file: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let out = run_cmd(Command::new("git").arg("-C").arg(root).args([
        "grep",
        "-E",
        "SYS_[a-z0-9_]+|__NR_[a-z0-9_]+|Sysno::[a-z0-9_]+",
        "--",
        "tests",
        "hermit-cli/tests",
        "detcore/tests",
    ]));
    if let Ok(out) = out {
        for line in out.lines() {
            let Some((file, text)) = line.split_once(':') else {
                continue;
            };
            for name in names {
                if text.contains(&format!("SYS_{name}"))
                    || text.contains(&format!("__NR_{name}"))
                    || text.contains(&format!("Sysno::{name}"))
                {
                    per_file
                        .entry(name.clone())
                        .or_default()
                        .insert(file.to_string());
                }
            }
        }
    }
    names
        .iter()
        .map(|n| {
            (
                n.clone(),
                per_file.get(n).map(|s| s.len() as u32).unwrap_or(0),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Loading pages, aggregate outputs, and the facts comparison.
// ---------------------------------------------------------------------------

fn load_pages(root: &Path) -> Vec<(String, String, Report, Option<Page>)> {
    let dir = root.join(PAGES_DIR);
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".md") && n != "README.md" && n != "SCHEMA.md")
        .map(|n| n.trim_end_matches(".md").to_string())
        .collect();
    names.sort();
    for name in names {
        let text = read(&dir.join(format!("{name}.md")));
        let (rep, page) = lint_page(&name, &text, root);
        out.push((name, text, rep, page));
    }
    out
}

fn syscalls_json(pages: &[Page], sha: &str) -> String {
    let rows: Vec<serde_json::Value> = pages
        .iter()
        .map(|p| {
            let f = &p.code_facts;
            serde_json::json!({
                "syscall": p.syscall,
                "nr_x86_64": p.nr_x86_64,
                "strategy": strategy(f),
                "detcore_class": f.detcore_class,
                "refused": f.refused,
                "refused_only_when_fail_closed": f.refused_only_when_fail_closed,
                "dispatch": f.dispatch,
                "subscribed_run": f.subscribed_run,
                "subscribed_record_replay": f.subscribed_record_replay,
                "linux_determinism": p.linux_determinism,
                "target_policy": p.target_policy,
                "tracking_status": p.tracking_status,
                "cross_host_variant": p.cross_host_variant,
                "discussion": {"kind": p.discussion_kind, "url": p.discussion_url},
                "page": format!("docs/syscalls/{}.md", p.syscall),
            })
        })
        .collect();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for p in pages {
        *counts.entry(strategy(&p.code_facts)).or_default() += 1;
    }
    let doc = serde_json::json!({
        "schema": "syscalls.json/v1",
        "generated_by": "scripts/syscall-pages.rs aggregate",
        "code_facts_as_of": sha,
        "total": pages.len(),
        "counts": counts,
        "syscalls": rows,
    });
    serde_json::to_string_pretty(&doc).unwrap() + "\n"
}

fn index_md(pages: &[Page], sha: &str) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for p in pages {
        *counts.entry(strategy(&p.code_facts)).or_default() += 1;
    }
    let mut s = String::new();
    let _ = writeln!(s, "# Per-syscall Detcore pages");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "One page per Linux syscall in the pinned x86-64 table: what Detcore does with it, and why. The machine-readable form is [syscalls.json](syscalls.json); the page format is [SCHEMA.md](SCHEMA.md). Regenerate both with `scripts/syscall-pages.rs aggregate`; verify with `scripts/syscall-pages.rs check`."
    );
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "These pages track Detcore strategy only. Which backends can run a syscall is tracked with the backends, not here. Which syscalls the test matrix exercises is a coverage measurement linked from the syscall-coverage audit, not a column below."
    );
    let _ = writeln!(s);
    let _ = writeln!(s, "Code facts as of hermit `{sha}`.");
    let _ = writeln!(s);
    let _ = writeln!(s, "| strategy | count |");
    let _ = writeln!(s, "|---|---:|");
    for (k, v) in &counts {
        let _ = writeln!(s, "| {k} | {v} |");
    }
    let _ = writeln!(s, "| **total** | **{}** |", pages.len());
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "| syscall | nr | strategy | dispatch | linux determinism | target policy | status | discussion |"
    );
    let _ = writeln!(s, "|---|---:|---|---|---|---|---|---|");
    for p in pages {
        let _ = writeln!(
            s,
            "| [{}]({}.md) | {} | {} | {} | {} | {} | {} | [{}]({}) |",
            p.syscall,
            p.syscall,
            p.nr_x86_64,
            strategy(&p.code_facts),
            p.code_facts.dispatch,
            p.linux_determinism,
            p.target_policy,
            p.tracking_status,
            p.discussion_kind,
            p.discussion_url
        );
    }
    s
}

fn facts_diff(
    pages: &[Page],
    sha: &str,
    rows: &BTreeMap<String, FactRow>,
    dispatch: &BTreeMap<String, String>,
    naming: &BTreeMap<String, u32>,
) -> Vec<String> {
    let mut out = Vec::new();
    let want = |p: &Page| CodeFacts {
        as_of: sha.to_string(),
        detcore_class: rows[&p.syscall].detcore_class.clone(),
        dispatch: dispatch.get(&p.syscall).cloned().unwrap_or_default(),
        refused: rows[&p.syscall].refused,
        refused_only_when_fail_closed: rows[&p.syscall].refused_only_when_fail_closed,
        subscribed_run: rows[&p.syscall].subscribed_run,
        subscribed_record_replay: rows[&p.syscall].subscribed_record_replay,
        tests_naming: naming.get(&p.syscall).copied().unwrap_or(0),
    };
    for p in pages {
        if !rows.contains_key(&p.syscall) {
            out.push(format!(
                "{}: page has no facts row (not a pinned Sysno?)",
                p.syscall
            ));
            continue;
        }
        let w = want(p);
        let f = &p.code_facts;
        let mut diffs = Vec::new();
        if f.detcore_class != w.detcore_class {
            diffs.push(format!(
                "detcore_class {} -> {}",
                f.detcore_class, w.detcore_class
            ));
        }
        if f.dispatch != w.dispatch {
            diffs.push(format!("dispatch {} -> {}", f.dispatch, w.dispatch));
        }
        if f.refused != w.refused {
            diffs.push(format!("refused {} -> {}", f.refused, w.refused));
        }
        if f.refused_only_when_fail_closed != w.refused_only_when_fail_closed {
            diffs.push("refused_only_when_fail_closed changed".into());
        }
        if f.subscribed_run != w.subscribed_run {
            diffs.push("subscribed_run changed".into());
        }
        if f.subscribed_record_replay != w.subscribed_record_replay {
            diffs.push("subscribed_record_replay changed".into());
        }
        if f.tests_naming != w.tests_naming {
            diffs.push(format!(
                "tests_naming {} -> {}",
                f.tests_naming, w.tests_naming
            ));
        }
        if p.nr_x86_64 != rows[&p.syscall].nr {
            diffs.push(format!(
                "nr_x86_64 {} -> {}",
                p.nr_x86_64, rows[&p.syscall].nr
            ));
        }
        if !diffs.is_empty() {
            out.push(format!("{}: {}", p.syscall, diffs.join(", ")));
        }
    }
    for name in rows.keys() {
        if !pages.iter().any(|p| &p.syscall == name) {
            out.push(format!("{name}: pinned Sysno has no page"));
        }
    }
    out
}

fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    // Howard Hinnant's civil-from-days algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

fn discussion_url(name: &str) -> String {
    let page =
        format!("https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/{name}.md");
    let title = url_encode(&format!("Syscall discussion: {name}"));
    let body = url_encode(&format!(
        "Discussion for the `{name}` syscall page: {page}\n\nWhat should change in how Detcore handles `{name}`, or in what the page says?\n"
    ));
    format!("https://github.com/facebookexperimental/hermit/issues/new?title={title}&body={body}")
}

fn fix_done_criteria(t: &str) -> String {
    t.replace(
        "each such test exists at the cited hermit SHA and runs on ptrace in CI; the code realizes `target_policy`; and `ptrace_verification` is `verified`.",
        "each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence.",
    )
    .replace(
        "If a problem is later found with this syscall, the issue reopens: `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.",
        "If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.",
    )
    .replace("Closed means `done` (owner rule, 2026-10-02):", "`done` means (owner rule, 2026-10-02):")
}

/// One scalar from a v1 Tracking-data YAML block (top level or indented).
fn v1_grab(yaml: &str, key: &str) -> Option<String> {
    for line in yaml.lines() {
        let t = line.trim_start();
        if let Some(v) = t.strip_prefix(&format!("{key}:")) {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn convert_body(
    name: &str,
    text: &str,
    mapping: &BTreeMap<String, Vec<u64>>,
    facts: &BTreeMap<String, FactRow>,
    dispatch: &BTreeMap<String, String>,
    naming: &BTreeMap<String, u32>,
    sha: &str,
    date: &str,
) -> Result<Page, String> {
    let yaml_start = text.find("```yaml\n").ok_or("no v1 yaml block")? + 8;
    let yaml_end = text[yaml_start..]
        .find("```")
        .ok_or("unterminated v1 yaml block")?
        + yaml_start;
    let yaml = &text[yaml_start..yaml_end];
    let fact = facts
        .get(name)
        .ok_or_else(|| format!("{name} has no facts row"))?;
    let grab = |k: &str| v1_grab(yaml, k).unwrap_or_default();
    let mut related: Vec<String> = Vec::new();
    for n in mapping.get(name).cloned().unwrap_or_default() {
        related.push(format!("https://github.com/rrnewton/hermit/issues/{n}"));
    }
    // The ppoll one-off tracker used a different title pattern, so it is in
    // no plan file: https://github.com/rrnewton/hermit/issues/26.
    if name == "ppoll" {
        related.push("https://github.com/rrnewton/hermit/issues/26".into());
    }
    let body = &text[yaml_end + 3..];
    let mut sections = split_sections(body);
    sections.remove("Tracking data");
    if let Some(done) = sections.get("Done criteria").cloned() {
        sections.insert("Done criteria".into(), fix_done_criteria(&done));
    }
    for (h, b) in sections.iter_mut() {
        *b = scrub_text(b);
        // The dropped verification key survives only in historical Status
        // entries; live prose says the same thing without naming the key.
        if h != "Status" {
            *b = b
                .replace(
                    "`ptrace_verification` stays `untested`",
                    "verification on the default backend stays untested",
                )
                .replace("`ptrace_verification`", "verification")
                .replace("ptrace_verification", "verification");
        }
    }
    let disc = discussion_url(name);
    sections.insert(
        "Discussion".into(),
        format!("\n[Start a discussion about `{name}`]({disc}) (opens a prefilled issue form; nothing is filed until you submit it).\n"),
    );
    let entry = format!(
        "- {date} · hermit {} · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.",
        &sha[..12.min(sha.len())]
    );
    let status = sections.entry("Status".into()).or_default();
    if !status.trim_end().ends_with(&entry) {
        status.push_str(&entry);
        status.push('\n');
    }
    Ok(Page {
        syscall: name.to_string(),
        nr_x86_64: fact.nr,
        linux_determinism: grab("linux_determinism"),
        target_policy: grab("target_policy"),
        tracking_status: grab("tracking_status"),
        cross_host_variant: grab("cross_host_variant") == "true",
        related,
        discussion_kind: "new".into(),
        discussion_url: disc,
        code_facts: CodeFacts {
            as_of: sha.to_string(),
            detcore_class: fact.detcore_class.clone(),
            dispatch: dispatch.get(name).cloned().unwrap_or_default(),
            refused: fact.refused,
            refused_only_when_fail_closed: fact.refused_only_when_fail_closed,
            subscribed_run: fact.subscribed_run,
            subscribed_record_replay: fact.subscribed_record_replay,
            tests_naming: naming.get(name).copied().unwrap_or(0),
        },
        sections,
    })
}

// ---------------------------------------------------------------------------
// Commands.
// ---------------------------------------------------------------------------

fn print_reports(loaded: &[(String, String, Report, Option<Page>)]) -> usize {
    let mut errors = 0;
    for (name, _, rep, _) in loaded {
        for e in &rep.errors {
            eprintln!("{name}: {e}");
            errors += 1;
        }
        for w in &rep.warnings {
            println!("{name}: warning {w}");
        }
    }
    errors
}

fn good_pages(loaded: &[(String, String, Report, Option<Page>)]) -> Vec<Page> {
    loaded.iter().filter_map(|(_, _, _, p)| p.clone()).collect()
}

fn dispatch_at(root: &Path, classes: &BTreeMap<String, String>) -> DispatchReport {
    let lib = read(&root.join("detcore/src/lib.rs"));
    let cls = read(&root.join("detcore/src/syscall_classification.rs"));
    parse_dispatch(
        &lib,
        &[
            ("detcore/src/lib.rs", &lib),
            ("detcore/src/syscall_classification.rs", &cls),
        ],
        classes,
    )
}

fn cmd_lint(root: &Path) -> i32 {
    let loaded = load_pages(root);
    if loaded.is_empty() {
        eprintln!("syscall-pages: no pages under {PAGES_DIR}");
        return 1;
    }
    let errors = print_reports(&loaded);
    println!("lint: {} pages, {errors} errors", loaded.len());
    if errors > 0 { 1 } else { 0 }
}

fn facts_context(
    root: &Path,
    pages: &[Page],
) -> Result<
    (
        String,
        BTreeMap<String, FactRow>,
        DispatchReport,
        BTreeMap<String, u32>,
    ),
    String,
> {
    let (sha, rows) = export_facts(root)?;
    let classes: BTreeMap<String, String> = rows
        .iter()
        .map(|(k, v)| (k.clone(), v.detcore_class.clone()))
        .collect();
    let dispatch = dispatch_at(root, &classes);
    if dispatch.status != "ok" {
        return Err(format!(
            "dispatch parse failed: {}",
            dispatch.errors.join("; ")
        ));
    }
    let names: BTreeSet<String> = rows.keys().cloned().collect();
    let naming = tests_naming(root, &names);
    let _ = pages;
    Ok((sha, rows, dispatch, naming))
}

fn cmd_facts(root: &Path, apply: bool) -> i32 {
    let loaded = load_pages(root);
    let pages = good_pages(&loaded);
    let (sha, rows, dispatch, naming) = match facts_context(root, &pages) {
        Ok(v) => v,
        Err(e) => fail(&e),
    };
    let diffs = facts_diff(&pages, &sha, &rows, &dispatch.dispatch, &naming);
    if !apply {
        for d in &diffs {
            println!("{d}");
        }
        println!(
            "facts at {sha}: {} pages differ (dispatch parse {}; missing arms: {})",
            diffs.len(),
            dispatch.status,
            dispatch.missing_arm.len()
        );
        return if diffs.is_empty() { 0 } else { 1 };
    }
    let date = today_utc();
    let mut changed = 0;
    for p in &pages {
        let Some(row) = rows.get(&p.syscall) else {
            continue;
        };
        let want = CodeFacts {
            as_of: sha.clone(),
            detcore_class: row.detcore_class.clone(),
            dispatch: dispatch
                .dispatch
                .get(&p.syscall)
                .cloned()
                .unwrap_or_default(),
            refused: row.refused,
            refused_only_when_fail_closed: row.refused_only_when_fail_closed,
            subscribed_run: row.subscribed_run,
            subscribed_record_replay: row.subscribed_record_replay,
            tests_naming: naming.get(&p.syscall).copied().unwrap_or(0),
        };
        let values_differ = p.code_facts.detcore_class != want.detcore_class
            || p.code_facts.dispatch != want.dispatch
            || p.code_facts.refused != want.refused
            || p.code_facts.refused_only_when_fail_closed != want.refused_only_when_fail_closed
            || p.code_facts.subscribed_run != want.subscribed_run
            || p.code_facts.subscribed_record_replay != want.subscribed_record_replay
            || p.code_facts.tests_naming != want.tests_naming;
        if !values_differ {
            continue;
        }
        let mut q = p.clone();
        q.code_facts = want;
        let entry = format!(
            "- {date} · hermit {} · syscall-pages · facts: code_facts refreshed by `facts --apply` at this SHA.",
            &sha[..12]
        );
        let status = q.sections.entry("Status".into()).or_default();
        status.push_str(&entry);
        status.push('\n');
        let path = root.join(PAGES_DIR).join(format!("{}.md", p.syscall));
        if fs::write(&path, emit_page(&q)).is_err() {
            fail(&format!("cannot write {}", path.display()));
        }
        changed += 1;
    }
    println!("facts --apply at {sha}: {changed} pages updated");
    0
}

fn aggregate_sha(pages: &[Page]) -> String {
    let mut set = BTreeSet::new();
    for p in pages {
        set.insert(p.code_facts.as_of.clone());
    }
    if set.len() == 1 {
        set.into_iter().next().unwrap()
    } else {
        "mixed (see pages)".into()
    }
}

fn cmd_aggregate(root: &Path, check_only: bool) -> i32 {
    let loaded = load_pages(root);
    let errors = print_reports(&loaded);
    if errors > 0 {
        eprintln!("syscall-pages: aggregate refuses while lint has {errors} errors");
        return 1;
    }
    let mut pages = good_pages(&loaded);
    pages.sort_by_key(|p| p.nr_x86_64);
    let sha = aggregate_sha(&pages);
    let json = syscalls_json(&pages, &sha);
    let index = index_md(&pages, &sha);
    let json_path = root.join(JSON_FILE);
    let index_path = root.join(INDEX_FILE);
    if check_only {
        let mut bad = 0;
        if read(&json_path) != json {
            eprintln!(
                "syscall-pages: {JSON_FILE} is stale; run `scripts/syscall-pages.rs aggregate`"
            );
            bad += 1;
        }
        if read(&index_path) != index {
            eprintln!(
                "syscall-pages: {INDEX_FILE} is stale; run `scripts/syscall-pages.rs aggregate`"
            );
            bad += 1;
        }
        return if bad > 0 { 1 } else { 0 };
    }
    if fs::write(&json_path, json).is_err() || fs::write(&index_path, index).is_err() {
        fail("cannot write aggregate outputs");
    }
    println!(
        "aggregate: {} pages -> {JSON_FILE}, {INDEX_FILE}",
        pages.len()
    );
    0
}

fn cmd_check(root: &Path, quick: bool) -> i32 {
    let mut rc = 0;
    let loaded = load_pages(root);
    if loaded.is_empty() {
        eprintln!("syscall-pages: no pages under {PAGES_DIR}");
        return 1;
    }
    let errors = print_reports(&loaded);
    if errors > 0 {
        rc = 1;
    }
    let pages = good_pages(&loaded);
    {
        let mut sorted = pages.clone();
        sorted.sort_by_key(|p| p.nr_x86_64);
        let sha = aggregate_sha(&sorted);
        if read(&root.join(JSON_FILE)) != syscalls_json(&sorted, &sha) {
            eprintln!("check: {JSON_FILE} is stale; run `scripts/syscall-pages.rs aggregate`");
            rc = 1;
        }
        if read(&root.join(INDEX_FILE)) != index_md(&sorted, &sha) {
            eprintln!("check: {INDEX_FILE} is stale; run `scripts/syscall-pages.rs aggregate`");
            rc = 1;
        }
    }
    if quick {
        let classes: BTreeMap<String, String> = pages
            .iter()
            .map(|p| (p.syscall.clone(), p.code_facts.detcore_class.clone()))
            .collect();
        let dispatch = dispatch_at(root, &classes);
        if dispatch.status != "ok" {
            eprintln!(
                "check: dispatch parser-failed: {}",
                dispatch.errors.join("; ")
            );
            rc = 1;
        } else {
            for name in &dispatch.missing_arm {
                eprintln!("check: {name}: determinized but reaches the fallback arm");
                rc = 1;
            }
            for p in &pages {
                if let Some(d) = dispatch.dispatch.get(&p.syscall) {
                    if d != &p.code_facts.dispatch {
                        eprintln!(
                            "check: {}: dispatch {} disagrees with source parse {d}",
                            p.syscall, p.code_facts.dispatch
                        );
                        rc = 1;
                    }
                }
            }
        }
        println!("check --quick: {} pages (facts build skipped)", pages.len());
        return rc;
    }
    match facts_context(root, &pages) {
        Ok((sha, rows, dispatch, naming)) => {
            let diffs = facts_diff(&pages, &sha, &rows, &dispatch.dispatch, &naming);
            for d in &diffs {
                eprintln!("check: {d}");
            }
            if !diffs.is_empty() {
                rc = 1;
            }
            println!(
                "check: {} pages at {sha}; {} facts diffs",
                pages.len(),
                diffs.len()
            );
        }
        Err(e) => {
            eprintln!("check: {e}");
            rc = 1;
        }
    }
    rc
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

fn cmd_convert(root: &Path, args: &[String]) -> i32 {
    let from = arg_value(args, "--from")
        .unwrap_or_else(|| fail("convert needs --from DIR (recovered v1 bodies)"));
    let mapping_path =
        arg_value(args, "--mapping").unwrap_or_else(|| fail("convert needs --mapping JSON"));
    let only: BTreeSet<String> = args
        .iter()
        .skip_while(|a| *a != "--only")
        .skip(1)
        .take_while(|a| !a.starts_with("--"))
        .cloned()
        .collect();
    let mapping_text = read(Path::new(&mapping_path));
    let mapping_v: serde_json::Value = serde_json::from_str(&mapping_text)
        .unwrap_or_else(|e| fail(&format!("bad --mapping JSON: {e}")));
    let mut mapping: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    if let Some(obj) = mapping_v.as_object() {
        for (k, v) in obj {
            let nums = v
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
                .unwrap_or_default();
            mapping.insert(k.clone(), nums);
        }
    }
    let (sha, rows) = match export_facts(root) {
        Ok(v) => v,
        Err(e) => fail(&e),
    };
    let classes: BTreeMap<String, String> = rows
        .iter()
        .map(|(k, v)| (k.clone(), v.detcore_class.clone()))
        .collect();
    let dispatch = dispatch_at(root, &classes);
    if dispatch.status != "ok" {
        fail(&format!(
            "dispatch parse failed: {}",
            dispatch.errors.join("; ")
        ));
    }
    let names: BTreeSet<String> = rows.keys().cloned().collect();
    let naming = tests_naming(root, &names);
    let date = arg_value(args, "--date").unwrap_or_else(today_utc);
    let out_dir = root.join(PAGES_DIR);
    if fs::create_dir_all(&out_dir).is_err() {
        fail("cannot create docs/syscalls");
    }
    let mut written = 0;
    let mut failed = 0;
    for name in rows.keys() {
        if !only.is_empty() && !only.contains(name) {
            continue;
        }
        let src = Path::new(&from).join(format!("{name}.md"));
        if !src.exists() {
            eprintln!("convert: {name}: no recovered body at {}", src.display());
            failed += 1;
            continue;
        }
        let text = read(&src);
        match convert_body(
            name,
            &text,
            &mapping,
            &rows,
            &dispatch.dispatch,
            &naming,
            &sha,
            &date,
        ) {
            Ok(page) => {
                let out = out_dir.join(format!("{name}.md"));
                if fs::write(&out, emit_page(&page)).is_err() {
                    fail(&format!("cannot write {}", out.display()));
                }
                written += 1;
            }
            Err(e) => {
                eprintln!("convert: {name}: {e}");
                failed += 1;
            }
        }
    }
    println!("convert: {written} pages written at {sha}, {failed} failed");
    if failed > 0 { 1 } else { 0 }
}

/// Escape text for HTML. Rendered pages are generated, never hand-edited.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Minimal inline Markdown on already-escaped text: `code`, **bold**,
/// [text](url). Anything unrecognised stays literal text.
fn inline_md(escaped: &str) -> String {
    let mut out = String::new();
    let mut rest = escaped;
    loop {
        let code = rest.find('`');
        let bold = rest.find("**");
        let link = rest.find('[');
        let next = [code, bold, link].into_iter().flatten().min();
        let Some(at) = next else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(stripped) = rest.strip_prefix('`') {
            match stripped.find('`') {
                Some(end) => {
                    out.push_str(&format!("<code>{}</code>", &stripped[..end]));
                    rest = &stripped[end + 1..];
                }
                None => {
                    out.push('`');
                    rest = stripped;
                }
            }
        } else if let Some(stripped) = rest.strip_prefix("**") {
            match stripped.find("**") {
                Some(end) => {
                    out.push_str(&format!("<strong>{}</strong>", &stripped[..end]));
                    rest = &stripped[end + 2..];
                }
                None => {
                    out.push_str("**");
                    rest = stripped;
                }
            }
        } else if rest.starts_with('[') {
            let parsed = rest.find("](").and_then(|mid| {
                rest[mid..].find(')').map(|close| {
                    (
                        rest[1..mid].to_string(),
                        rest[mid + 2..mid + close].to_string(),
                        mid + close + 1,
                    )
                })
            });
            match parsed {
                Some((label, url, len))
                    if url.starts_with("https://")
                        || url.ends_with(".md")
                        || url.ends_with(".html") =>
                {
                    let url = url.replace(".md", ".html");
                    out.push_str(&format!("<a href=\"{}\">{}</a>", html_escape(&url), label));
                    rest = &rest[len..];
                }
                _ => {
                    out.push('[');
                    rest = &rest[1..];
                }
            }
        }
    }
    out
}

/// Render one section body. Deliberately small: headings, fenced code,
/// bullet lists (indentation becomes nested text), and paragraphs cover the
/// page corpus; anything else degrades to paragraph text rather than
/// breaking the page.
fn md_to_html(body: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    let mut in_list = false;
    let mut para: Vec<String> = Vec::new();
    let flush_para = |out: &mut String, para: &mut Vec<String>| {
        if !para.is_empty() {
            let text = para.join(" ");
            let _ = writeln!(out, "<p>{}</p>", inline_md(&html_escape(&text)));
            para.clear();
        }
    };
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            flush_para(&mut out, &mut para);
            if in_list {
                out.push_str("</ul>\n");
                in_list = false;
            }
            out.push_str(if in_fence {
                "</code></pre>\n"
            } else {
                "<pre><code>"
            });
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.push_str(&html_escape(line));
            out.push('\n');
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush_para(&mut out, &mut para);
            if in_list {
                out.push_str("</ul>\n");
                in_list = false;
            }
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("### ") {
            flush_para(&mut out, &mut para);
            if in_list {
                out.push_str("</ul>\n");
                in_list = false;
            }
            let _ = writeln!(out, "<h3>{}</h3>", inline_md(&html_escape(h)));
            continue;
        }
        if trimmed == "---" {
            flush_para(&mut out, &mut para);
            out.push_str("<hr>\n");
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            flush_para(&mut out, &mut para);
            if !in_list {
                out.push_str("<ul>\n");
                in_list = true;
            }
            let _ = writeln!(out, "<li>{}</li>", inline_md(&html_escape(item)));
            continue;
        }
        if in_list && line.starts_with("  ") {
            // Continuation of the previous list item.
            if let Some(pos) = out.rfind("</li>") {
                out.insert_str(pos, &format!(" {}", inline_md(&html_escape(trimmed))));
            }
            continue;
        }
        para.push(trimmed.to_string());
    }
    flush_para(&mut out, &mut para);
    if in_list {
        out.push_str("</ul>\n");
    }
    if in_fence {
        out.push_str("</code></pre>\n");
    }
    out
}

const SITE_STYLE: &str = "body{font-family:system-ui,sans-serif;max-width:70rem;margin:2rem auto;padding:0 1rem;line-height:1.55}table{border-collapse:collapse;width:100%}th,td{border:1px solid #8884;padding:.3rem .5rem;text-align:left}input,select{font:inherit;padding:.4rem;margin:0 .5rem 1rem 0}code{background:#8882;padding:.05rem .25rem;border-radius:.25rem}pre{background:#8881;padding:1rem;overflow:auto}";

fn site_index_html(pages: &[Page], sha: &str) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for p in pages {
        *counts.entry(strategy(&p.code_facts)).or_default() += 1;
    }
    let mut s = String::new();
    let _ = writeln!(
        s,
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">"
    );
    let _ = writeln!(
        s,
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
    );
    let _ = writeln!(s, "<title>Per-syscall Detcore pages | Hermit</title>");
    let _ = writeln!(
        s,
        "<link rel=\"stylesheet\" href=\"../site.css\"><style>{SITE_STYLE}</style></head><body>"
    );
    let _ = writeln!(
        s,
        "<header><p><a href=\"../\">Hermetic Infra</a></p><h1>Per-syscall Detcore pages</h1>"
    );
    let _ = writeln!(
        s,
        "<p>What Detcore does with each Linux syscall, and why. Code facts as of hermit <code>{}</code>. The machine-readable table is <a href=\"https://github.com/facebookexperimental/hermit/blob/main/docs/syscalls/syscalls.json\">syscalls.json</a>.</p></header>",
        html_escape(sha)
    );
    let _ = writeln!(s, "<p>");
    for (k, v) in &counts {
        let _ = write!(s, "{}: {} &middot; ", html_escape(k), v);
    }
    let _ = writeln!(s, "total: {}</p>", pages.len());
    let _ = writeln!(
        s,
        "<label>Search <input id=\"q\" type=\"search\" placeholder=\"syscall, strategy, dispatch...\"></label>"
    );
    let _ = writeln!(
        s,
        "<label>Strategy <select id=\"s\"><option value=\"\">all</option>"
    );
    for k in counts.keys() {
        let _ = writeln!(s, "<option>{}</option>", html_escape(k));
    }
    let _ = writeln!(s, "</select></label>");
    let _ = writeln!(
        s,
        "<table><thead><tr><th>syscall</th><th>nr</th><th>strategy</th><th>dispatch</th><th>linux determinism</th><th>target policy</th><th>status</th><th>discussion</th></tr></thead><tbody id=\"rows\">"
    );
    for p in pages {
        let _ = writeln!(
            s,
            "<tr data-strategy=\"{}\"><td><a href=\"{}.html\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><a href=\"{}\">{}</a></td></tr>",
            html_escape(strategy(&p.code_facts)),
            html_escape(&p.syscall),
            html_escape(&p.syscall),
            p.nr_x86_64,
            html_escape(strategy(&p.code_facts)),
            html_escape(&p.code_facts.dispatch),
            html_escape(&p.linux_determinism),
            html_escape(&p.target_policy),
            html_escape(&p.tracking_status),
            html_escape(&p.discussion_url),
            html_escape(&p.discussion_kind),
        );
    }
    let _ = writeln!(s, "</tbody></table>");
    let _ = writeln!(
        s,
        "<script>const q=document.getElementById('q'),s=document.getElementById('s'),rows=[...document.querySelectorAll('#rows tr')];function f(){{const t=q.value.toLowerCase(),v=s.value;for(const r of rows){{r.style.display=(!v||r.dataset.strategy===v)&&r.textContent.toLowerCase().includes(t)?'':'none';}}}}q.addEventListener('input',f);s.addEventListener('change',f);</script>"
    );
    let _ = writeln!(s, "</body></html>");
    s
}

fn site_page_html(page: &Page) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">"
    );
    let _ = writeln!(
        s,
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
    );
    let _ = writeln!(
        s,
        "<title>{} | Per-syscall Detcore pages</title>",
        html_escape(&page.syscall)
    );
    let _ = writeln!(
        s,
        "<link rel=\"stylesheet\" href=\"../site.css\"><style>{SITE_STYLE}</style></head><body>"
    );
    let _ = writeln!(
        s,
        "<header><p><a href=\"./\">All syscalls</a></p><h1><code>{}</code> ({})</h1>",
        html_escape(&page.syscall),
        page.nr_x86_64
    );
    let _ = writeln!(
        s,
        "<p>Strategy: <strong>{}</strong> &middot; dispatch: <code>{}</code> &middot; linux determinism: {} &middot; target policy: {} &middot; status: {} &middot; <a href=\"{}\">discussion ({})</a></p></header>",
        html_escape(strategy(&page.code_facts)),
        html_escape(&page.code_facts.dispatch),
        html_escape(&page.linux_determinism),
        html_escape(&page.target_policy),
        html_escape(&page.tracking_status),
        html_escape(&page.discussion_url),
        html_escape(&page.discussion_kind),
    );
    for h in SECTIONS {
        let _ = writeln!(s, "<h2>{}</h2>", html_escape(h));
        if let Some(body) = page.sections.get(h) {
            s.push_str(&md_to_html(body));
        }
    }
    let _ = writeln!(s, "</body></html>");
    s
}

fn cmd_render(root: &Path) -> i32 {
    let loaded = load_pages(root);
    let errors = print_reports(&loaded);
    if errors > 0 {
        eprintln!("syscall-pages: render refuses while lint has {errors} errors");
        return 1;
    }
    let mut pages = good_pages(&loaded);
    pages.sort_by_key(|p| p.nr_x86_64);
    let sha = aggregate_sha(&pages);
    let out = root.join("docs/site/syscalls");
    if fs::create_dir_all(&out).is_err() {
        fail("cannot create docs/site/syscalls");
    }
    if fs::write(out.join("index.html"), site_index_html(&pages, &sha)).is_err() {
        fail("cannot write site index");
    }
    for p in &pages {
        if fs::write(out.join(format!("{}.html", p.syscall)), site_page_html(p)).is_err() {
            fail(&format!("cannot write site page for {}", p.syscall));
        }
    }
    println!("render: {} pages -> docs/site/syscalls/", pages.len());
    0
}

fn usage() -> ! {
    eprintln!(
        "usage: syscall-pages.rs <lint|facts [--apply]|aggregate [--check]|check [--quick]|render|convert --from DIR --mapping JSON [--date YYYY-MM-DD] [--only NAME...]>"
    );
    exit(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_page() -> String {
        let mut sections = BTreeMap::new();
        sections.insert(
            "Summary".into(),
            "`read` reads bytes. Detcore models the deterministic parts.\n".into(),
        );
        sections.insert(
            "Linux semantics".into(),
            "Reads from a descriptor.\n\n### Flags and options\n\n- F1: the descriptor is a pipe at end of file: returns 0\n".into(),
        );
        sections.insert(
            "Determinism analysis".into(),
            "Repeats given fixed file contents.\n".into(),
        );
        sections.insert("Hermit design".into(), "Modelled in Detcore.\n".into());
        sections.insert(
            "Recommended tests".into(),
            "- T1 (covers F1): read a pipe at end of file\n".into(),
        );
        sections.insert(
            "Done criteria".into(),
            "`done` means (owner rule, 2026-10-02): every flag covered.\n".into(),
        );
        sections.insert(
            "Status".into(),
            "- 2026-11-15 · hermit 0123456789ab · test · converted: sample.\n".into(),
        );
        sections.insert("Additional information".into(), "None.\n".into());
        let page = Page {
            syscall: "read".into(),
            nr_x86_64: 0,
            linux_determinism: "conditional".into(),
            target_policy: "model".into(),
            tracking_status: "todo".into(),
            cross_host_variant: false,
            related: vec![],
            discussion_kind: "new".into(),
            discussion_url: discussion_url("read"),
            code_facts: CodeFacts {
                as_of: "0123456789abcdef0123456789abcdef01234567".into(),
                detcore_class: "determinized".into(),
                dispatch: "typed".into(),
                refused: false,
                refused_only_when_fail_closed: false,
                subscribed_run: true,
                subscribed_record_replay: true,
                tests_naming: 1,
            },
            sections,
        };
        let mut s = emit_page(&page);
        // Discussion section is authored prose, generated here for the fixture.
        s = s.replace(
            "## Discussion\n",
            &format!(
                "## Discussion\n\n[Start a discussion]({}) (prefilled form).\n",
                page.discussion_url
            ),
        );
        s
    }

    #[test]
    fn emitted_page_lints_clean_and_round_trips() {
        let text = sample_page();
        let (rep, page) = lint_page("read", &text, Path::new("."));
        assert!(rep.errors.is_empty(), "errors: {:?}", rep.errors);
        let page = page.expect("page parses");
        assert_eq!(page.syscall, "read");
        assert_eq!(strategy(&page.code_facts), "determinized");
    }

    #[test]
    fn lint_rejects_internal_hostname_and_unknown_key() {
        let bad = sample_page().replace("Reads from a descriptor.", "Measured on devbig030 today.");
        let (rep, _) = lint_page("read", &bad, Path::new("."));
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P30")),
            "{:?}",
            rep.errors
        );
        let bad = sample_page().replace(
            "tracking_status: todo",
            "tracking_status: todo\nptrace_verification: untested",
        );
        let (rep, _) = lint_page("read", &bad, Path::new("."));
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P05")),
            "{:?}",
            rep.errors
        );
    }

    #[test]
    fn lint_rejects_undefined_flag_and_bad_status_date() {
        let bad = sample_page().replace("(covers F1)", "(covers F9)");
        let (rep, _) = lint_page("read", &bad, Path::new("."));
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P11")),
            "{:?}",
            rep.errors
        );
        let bad = sample_page().replace("2026-11-15", "15 Nov 2026");
        let (rep, _) = lint_page("read", &bad, Path::new("."));
        assert!(
            rep.errors.iter().any(|e| e.starts_with("P12")),
            "{:?}",
            rep.errors
        );
    }

    #[test]
    fn scrub_rewrites_measured_forms() {
        let out =
            scrub_text("Evidence was gathered on `devbig030` (Linux 6.17) under `/var/tmp/foo`.");
        assert!(!out.contains("devbig030"), "{out}");
        assert!(scrub_findings(&out).is_empty(), "{out}");
        // Forms measured in the recovered bodies after the first conversion:
        // a bare "devbig host", and a hostname inside a quoted strace buffer.
        let out = scrub_text(
            "On this devbig host, measured 2026-10-03, readv(3, [{iov_base=\"devbig030.atn3.f\", iov_len=16}], 1) = 16",
        );
        assert!(!out.contains("devbig"), "{out}");
        assert!(scrub_findings(&out).is_empty(), "{out}");
        assert!(!scrub_findings("readv(3, [{iov_base=\"devbig030.atn3.f\"}], 1)").is_empty());
    }

    #[test]
    fn dispatch_parse_assigns_each_arm_kind_and_fails_closed() {
        let lib = r#"
            let res = match classify_syscall(call.number()) {
                _ if something.is_some() => { todo!() }
                SyscallClassification::Determinized if call.number() == Sysno::rseq => { todo!() }
                SyscallClassification::Determinized if is_refused_family(call.number()) => { todo!() }
                SyscallClassification::Determinized => match call {
                    Syscall::Read(s) => { todo!() }
                    unexpected => { todo!() }
                },
                SyscallClassification::PassThrough => { todo!() }
                SyscallClassification::Unsupported => { todo!() }
            };
        "#;
        let preds = "pub(crate) const fn is_refused_family(sysno: Sysno) -> bool { matches!(sysno, Sysno::futex_waitv) }";
        let classes: BTreeMap<String, String> = [
            ("read".to_string(), "determinized".to_string()),
            ("rseq".to_string(), "determinized".to_string()),
            ("futex_waitv".to_string(), "determinized".to_string()),
            ("mkdir".to_string(), "passthrough".to_string()),
            ("restart_syscall".to_string(), "unsupported".to_string()),
        ]
        .into_iter()
        .collect();
        let rep = parse_dispatch(lib, &[("preds", preds)], &classes);
        assert_eq!(rep.status, "ok", "errors: {:?}", rep.errors);
        assert_eq!(rep.dispatch["read"], "typed");
        assert_eq!(rep.dispatch["rseq"], "sysno-guard");
        assert_eq!(rep.dispatch["futex_waitv"], "pred:is_refused_family");
        assert_eq!(rep.dispatch["mkdir"], "passthrough");
        let broken = lib.replace(
            "match classify_syscall(call.number()) {",
            "match something_else {",
        );
        let rep = parse_dispatch(&broken, &[("preds", preds)], &classes);
        assert_eq!(rep.status, "parser-failed");
        assert!(rep.dispatch.values().all(|v| v == "parser-failed"));
    }

    #[test]
    fn render_escapes_and_links() {
        let html = md_to_html(
            "A <b>raw</b> `code` and **bold** [page](read.md) [ext](https://example.com).\n\n- F1: item\n  continuation\n",
        );
        assert!(html.contains("&lt;b&gt;raw&lt;/b&gt;"), "{html}");
        assert!(html.contains("<code>code</code>"), "{html}");
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        assert!(html.contains("href=\"read.html\""), "{html}");
        assert!(html.contains("continuation</li>"), "{html}");
    }

    #[test]
    fn facts_tsv_and_strategy_round_trip() {
        let (sha, rows) = parse_facts_tsv("# hermit abc\nname\tnr\tclass\trefused\tstrict_only_refused\tsub_run\tsub_record_replay\nread\t0\tdeterminized\t0\t0\t1\t1\n").unwrap();
        assert_eq!(sha, "abc");
        assert_eq!(rows["read"].nr, 0);
        assert_eq!(today_utc().len(), 10);
    }
}

fn main() {
    rust_script_prelude::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = repo_root();
    let rc = match args.first().map(|s| s.as_str()) {
        Some("lint") => cmd_lint(&root),
        Some("facts") => cmd_facts(&root, args.iter().any(|a| a == "--apply")),
        Some("aggregate") => cmd_aggregate(&root, args.iter().any(|a| a == "--check")),
        Some("check") => cmd_check(&root, args.iter().any(|a| a == "--quick")),
        Some("convert") => cmd_convert(&root, &args[1..]),
        Some("render") => cmd_render(&root),
        _ => usage(),
    };
    exit(rc);
}
