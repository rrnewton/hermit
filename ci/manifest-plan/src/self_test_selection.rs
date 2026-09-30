//! Change-set selection for the tool self-tests (`selftest.<name>` nodes) that
//! run only when a change touches their trigger paths.
//!
//! A self-test with trigger paths ([`crate::validation_dag::ToolSelfTest`]'s
//! `run_when_changed`) runs when any path changed since the merge base with
//! `origin/main`, committed or not, lies under one of them ([`is_under`]). A self-test that
//! does not run prints one line containing [`NOT_RUN_MARKER`], so the node log
//! and the scheduler's one-line node summary both say it was not run.
//!
//! Selection errs toward running. Main always runs, and so does any change set
//! this module cannot resolve: a shallow clone, a missing `origin/main`, an
//! empty change set, or any git failure.

use std::collections::BTreeSet;
use std::path::Path;

use crate::git_environment::git_command;

/// Set to `all` to run every self-test whatever the change set is.
pub const SELECTION_ENV: &str = "HERMIT_SELFTEST_SELECTION";

/// The text that marks a self-test the selection did not run.
pub const NOT_RUN_MARKER: &str = " NOT RUN by file selection: ";

/// Whether a self-test runs, and why.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Run(String),
    Skip { base: String, changed: Vec<String> },
}

/// The one line printed for a self-test that is not run.
pub fn not_run_line(name: &str, base: &str, changed: &[String], triggers: &[&str]) -> String {
    const SHOWN: usize = 5;
    let mut shown = changed
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if changed.len() > SHOWN {
        shown.push_str(&format!(", and {} more", changed.len() - SHOWN));
    }
    format!(
        "self-test {name}{NOT_RUN_MARKER}none of the {} path(s) changed since merge base {base} \
         ({shown}) is under {}; main runs it, and {SELECTION_ENV}=all runs it here",
        changed.len(),
        triggers.join(" or ")
    )
}

/// Whether `path` is under `trigger`: inside it when the trigger is a
/// directory (it ends in `/`), otherwise the trigger itself or, for a
/// submodule, a path inside it. `agent-utils` is not a prefix of
/// `agent-utils-notes.md`.
pub fn is_under(path: &str, trigger: &str) -> bool {
    if trigger.ends_with('/') {
        return path.starts_with(trigger);
    }
    path == trigger
        || path
            .strip_prefix(trigger)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Decide from the resolved change set. Pure, so every rule is testable
/// without a repository.
pub fn decide(
    selection_env: Option<&str>,
    change_set: Result<(String, Vec<String>), String>,
    triggers: &[&str],
) -> Decision {
    match selection_env {
        None | Some("") => {}
        Some("all") => return Decision::Run(format!("{SELECTION_ENV}=all")),
        Some(other) => {
            return Decision::Run(format!(
                "{SELECTION_ENV}={other:?} is not a known value, so it runs"
            ));
        }
    }
    let (base, changed) = match change_set {
        Ok(change_set) => change_set,
        Err(reason) => return Decision::Run(reason),
    };
    if changed.is_empty() {
        return Decision::Run(format!(
            "no path changed since merge base {base}, so there is nothing to select on"
        ));
    }
    if let Some((path, trigger)) = changed.iter().find_map(|path| {
        triggers
            .iter()
            .find(|trigger| is_under(path, trigger))
            .map(|trigger| (path, trigger))
    }) {
        return Decision::Run(format!("changed path {path} is under {trigger}"));
    }
    Decision::Skip { base, changed }
}

/// The merge base with `origin/main` and every path changed since it,
/// committed or not. `Err` names why the change set could not be resolved;
/// the caller then runs the self-test.
pub fn change_set(root: &Path) -> Result<(String, Vec<String>), String> {
    let git = |args: &[&str]| -> Result<Vec<u8>, String> {
        let output = git_command()
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .map_err(|error| format!("cannot run git {}: {error}", args.join(" ")))?;
        if !output.status.success() {
            return Err(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(output.stdout)
    };
    let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).trim().to_string();
    let mains = text(git(&[
        "for-each-ref",
        "--contains",
        "HEAD",
        "--format=%(refname)",
        "refs/heads/main",
        "refs/remotes/*/main",
    ])?);
    if !mains.is_empty() {
        return Err(format!(
            "HEAD is on main ({}), and main runs every self-test",
            mains.split_whitespace().collect::<Vec<_>>().join(", ")
        ));
    }
    if text(git(&["rev-parse", "--is-shallow-repository"])?) != "false" {
        return Err("this clone is shallow, so its merge base cannot be trusted".into());
    }
    let base = text(git(&["merge-base", "HEAD", "refs/remotes/origin/main"])?);
    if base.len() != 40 || !base.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("git merge-base printed {base:?}, not a commit"));
    }
    let mut changed = BTreeSet::new();
    for listing in [
        // A rename lists both its old and its new path. A submodule counts
        // as changed when its commit moves, whatever `diff.ignoreSubmodules`
        // the clone sets.
        git(&[
            "diff",
            "--name-only",
            "--no-renames",
            "--ignore-submodules=dirty",
            "-z",
            &base,
            "--",
        ])?,
        git(&["ls-files", "--others", "--exclude-standard", "-z"])?,
    ] {
        for path in listing.split(|&b| b == 0).filter(|path| !path.is_empty()) {
            changed.insert(
                String::from_utf8(path.to_vec())
                    .map_err(|_| "git listed a changed path that is not UTF-8".to_string())?,
            );
        }
    }
    Ok((base, changed.into_iter().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRIGGERS: &[&str] = &["ci/compat-envelope/", "ci/manifest-plan/"];

    fn change(paths: &[&str]) -> Result<(String, Vec<String>), String> {
        Ok((
            "a".repeat(40),
            paths.iter().map(|path| path.to_string()).collect(),
        ))
    }

    #[test]
    fn a_change_outside_the_triggers_skips_and_says_so_in_one_line() {
        let decision = decide(None, change(&["detcore/src/scheduler.rs"]), TRIGGERS);
        assert_eq!(
            decision,
            Decision::Skip {
                base: "a".repeat(40),
                changed: vec!["detcore/src/scheduler.rs".into()],
            }
        );
        let line = not_run_line(
            "scorecard_commands",
            &"a".repeat(40),
            &["detcore/src/scheduler.rs".into()],
            TRIGGERS,
        );
        assert!(!line.contains('\n'));
        assert!(line.starts_with("self-test scorecard_commands NOT RUN by file selection: "));
        assert!(line.contains("ci/compat-envelope/ or ci/manifest-plan/"));
        assert!(line.contains("detcore/src/scheduler.rs"));
    }

    #[test]
    fn a_change_under_a_trigger_runs() {
        for path in [
            "ci/compat-envelope/scorecard.rs",
            "ci/compat-envelope/testdata/series-snapshot/README.md",
            "ci/manifest-plan/src/bin/test-harness.rs",
        ] {
            let decision = decide(None, change(&["detcore/src/lib.rs", path]), TRIGGERS);
            assert!(
                matches!(&decision, Decision::Run(reason) if reason.contains(path)),
                "{path}: {decision:?}"
            );
        }
        // A sibling whose name merely starts with a trigger's text is not
        // under it.
        assert!(matches!(
            decide(None, change(&["ci/compat-envelope-notes.md"]), TRIGGERS),
            Decision::Skip { .. }
        ));
        // A trigger without the trailing slash is one path: a file, or a
        // submodule and anything inside it.
        let paths = &["agent-utils", "Cargo.lock"];
        for path in ["agent-utils", "agent-utils/rs/lib.rs", "Cargo.lock"] {
            assert!(
                matches!(decide(None, change(&[path]), paths), Decision::Run(_)),
                "{path}"
            );
        }
        for path in ["agent-utils-notes.md", "Cargo.lock.orig", "ci/Cargo.lock"] {
            assert!(
                matches!(decide(None, change(&[path]), paths), Decision::Skip { .. }),
                "{path}"
            );
        }
    }

    /// A trigger that names no tracked path never fires, so a misspelt or
    /// moved input would silently stop selecting its self-test.
    #[test]
    fn every_trigger_names_a_tracked_path() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let tracked = tracked_paths(&root);
        let mut checked = 0;
        for tool in crate::validation_dag::TOOL_SELF_TESTS {
            for trigger in tool.run_when_changed.unwrap_or_default() {
                assert!(
                    tracked.iter().any(|path| is_under(path, trigger)),
                    "self-test {}: trigger {trigger} names no tracked path",
                    tool.name
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "no self-test has a trigger to check");
    }

    fn tracked_paths(root: &Path) -> Vec<String> {
        let output = git_command()
            .arg("-C")
            .arg(root)
            .args(["ls-files", "-z"])
            .output()
            .unwrap();
        assert!(output.status.success(), "git ls-files: {output:?}");
        output
            .stdout
            .split(|&b| b == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect()
    }

    /// `dir/../x` -> `x`, over `/`-separated repository paths.
    fn normalize(path: &str) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        parts.join("/")
    }

    /// What a Rust source names by path: `#[path = "..."]` module files, with
    /// any spacing, inside `cfg_attr` and as raw identifiers; bodyless
    /// `mod x;` declarations, inside inline modules too; and the files
    /// `include_str!`, `include_bytes!` and `include!` read, with any of the
    /// three delimiters. Comments, string literals and character literals are
    /// skipped, so text that only mentions these forms names nothing. A form
    /// whose file this scan does not resolve is `Unresolved`, with its line.
    #[derive(Debug, PartialEq)]
    enum Reference {
        /// A `#[path]` module's file.
        Module(String),
        /// A bodyless `mod name;` inside the inline modules `within`
        /// (`a/b` for `mod a { mod b { mod name; } }`), whose file is found
        /// by its name unless a `#[path]` attribute names it. `test_only`
        /// when it, or an inline module around it, carries `#[cfg(test)]`.
        Plain {
            name: String,
            within: String,
            test_only: bool,
        },
        /// An included file.
        Include(String),
        /// A form that names a file this scan does not resolve, at `line`: a
        /// `#[path]` or include whose value is not a string literal, a
        /// `#[path]` inside an inline module, a bodyless module inside an
        /// inline module that has a `#[path]`, a macro-generated
        /// `mod $name;`, a renamed include macro, or
        /// `#[debugger_visualizer]`. No file in the tree uses one today; each
        /// is refused rather than resolved.
        Unresolved { form: String, line: usize },
    }

    #[derive(Debug, PartialEq)]
    enum Token {
        Ident(String),
        Punct(char),
        Str(String),
    }

    /// The tokens of `source`, each with its 1-based line.
    fn rust_tokens(source: &str) -> (Vec<Token>, Vec<usize>) {
        let chars: Vec<char> = source.chars().collect();
        let at = |i: usize| chars.get(i).copied().unwrap_or('\0');
        // The line of `counted`, advanced to each token's start.
        let (mut line, mut counted) = (1, 0);
        let mut tokens = Vec::new();
        let mut lines = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let begin = i;
            let c = chars[i];
            if c.is_whitespace() {
                i += 1;
            } else if c == '/' && at(i + 1) == '/' {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            } else if c == '/' && at(i + 1) == '*' {
                let mut depth = 0;
                while i < chars.len() {
                    if chars[i] == '/' && at(i + 1) == '*' {
                        depth += 1;
                        i += 2;
                    } else if chars[i] == '*' && at(i + 1) == '/' {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            } else if c == '"' {
                let mut text = String::new();
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        text.push(chars[i]);
                        i += 1;
                    }
                    text.push(at(i));
                    i += 1;
                }
                i += 1;
                tokens.push(Token::Str(text));
            } else if c == '\'' {
                if at(i + 1) == '\\' {
                    i += 3;
                    while i < chars.len() && chars[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                } else if at(i + 2) == '\'' {
                    i += 3;
                } else {
                    // A lifetime or a label.
                    i += 1;
                }
            } else if c.is_alphanumeric() || c == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let ident: String = chars[start..i].iter().collect();
                let hashes = chars[i..].iter().take_while(|&&c| c == '#').count();
                let raw = matches!(ident.as_str(), "r" | "br" | "cr");
                if raw && at(i + hashes) == '"' {
                    let close: Vec<char> = std::iter::once('"')
                        .chain(std::iter::repeat_n('#', hashes))
                        .collect();
                    i += hashes + 1;
                    let start = i;
                    while i < chars.len() && !chars[i..].starts_with(&close) {
                        i += 1;
                    }
                    tokens.push(Token::Str(chars[start..i].iter().collect()));
                    i += close.len();
                } else if matches!(ident.as_str(), "b" | "c") && at(i) == '"' {
                    // The prefix of a byte or C string: the next pass reads it.
                } else if ident == "b" && at(i) == '\'' {
                    // The prefix of a byte character: the next pass skips it.
                } else if ident == "r"
                    && at(i) == '#'
                    && (at(i + 1).is_alphabetic() || at(i + 1) == '_')
                {
                    // A raw identifier: `r#path` is the identifier `path`.
                    i += 1;
                    let start = i;
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        i += 1;
                    }
                    tokens.push(Token::Ident(chars[start..i].iter().collect()));
                } else {
                    tokens.push(Token::Ident(ident));
                }
            } else {
                tokens.push(Token::Punct(c));
                i += 1;
            }
            // Each pass reads at most one token, which starts at `begin`.
            if lines.len() < tokens.len() {
                line += chars[counted..begin].iter().filter(|&&c| c == '\n').count();
                counted = begin;
                lines.push(line);
            }
        }
        (tokens, lines)
    }

    fn rust_references(source: &str) -> Vec<Reference> {
        let (tokens, lines) = rust_tokens(source);
        let unresolved = |i: usize, form: String| Reference::Unresolved {
            form,
            line: lines[i],
        };
        let ident =
            |i: usize, name: &str| matches!(tokens.get(i), Some(Token::Ident(n)) if n == name);
        let punct = |i: usize, c: char| tokens.get(i) == Some(&Token::Punct(c));
        // The index just past the group that opens at `open` (`(`, `[` or
        // `{`), or the end.
        let group_end = |open: usize| {
            let mut depth = 0;
            for (j, token) in tokens.iter().enumerate().skip(open) {
                match token {
                    Token::Punct('(' | '[' | '{') => depth += 1,
                    Token::Punct(')' | ']' | '}') => {
                        depth -= 1;
                        if depth == 0 {
                            return j + 1;
                        }
                    }
                    _ => {}
                }
            }
            tokens.len()
        };
        // The outer attributes of the item whose keyword is at `item`, read
        // backwards over its visibility: each as the range of its tokens.
        let attributes = |item: usize| {
            let mut j = item;
            if j > 0 && punct(j - 1, ')') {
                let mut depth = 0;
                let mut k = j;
                while k > 0 {
                    k -= 1;
                    match &tokens[k] {
                        Token::Punct(')') => depth += 1,
                        Token::Punct('(') => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                if k > 0 && ident(k - 1, "pub") {
                    j = k - 1;
                }
            } else if j > 0 && ident(j - 1, "pub") {
                j -= 1;
            }
            let mut found = Vec::new();
            while j > 0 && punct(j - 1, ']') {
                let mut depth = 0;
                let mut k = j;
                while k > 0 {
                    k -= 1;
                    match &tokens[k] {
                        Token::Punct(']') => depth += 1,
                        Token::Punct('[') => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                if k == 0 || !punct(k - 1, '#') {
                    break;
                }
                found.push(k - 1..j);
                j = k - 1;
            }
            found
        };
        let is_cfg_test = |range: &std::ops::Range<usize>| {
            range.len() == 7
                && ident(range.start + 2, "cfg")
                && punct(range.start + 3, '(')
                && ident(range.start + 4, "test")
                && punct(range.start + 5, ')')
        };
        let names_path = |range: &std::ops::Range<usize>| {
            range.clone().any(|j| ident(j, "path") && punct(j + 1, '='))
        };
        // The inline modules around the current token: name, whether a
        // `#[path]` or `#[cfg(test)]` is on it, and the index past its `}`.
        struct Inline {
            name: String,
            path: bool,
            test_only: bool,
            end: usize,
        }
        let mut inline: Vec<Inline> = Vec::new();
        let mut references = Vec::new();
        for i in 0..tokens.len() {
            while inline.last().is_some_and(|module| i >= module.end) {
                inline.pop();
            }
            if punct(i, '#')
                && punct(i + 1, '[')
                && (ident(i + 2, "path") || ident(i + 2, "cfg_attr"))
            {
                let mut depth = 0;
                for j in i + 1..tokens.len() {
                    match &tokens[j] {
                        Token::Punct('[' | '(') => depth += 1,
                        Token::Punct(']' | ')') => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        Token::Ident(name) if name == "path" && punct(j + 1, '=') => {
                            references.push(match (inline.last(), tokens.get(j + 2)) {
                                (Some(module), _) => unresolved(
                                    j,
                                    format!("#[path] inside the inline module {}", module.name),
                                ),
                                (None, Some(Token::Str(target))) => {
                                    Reference::Module(target.clone())
                                }
                                (None, _) => unresolved(
                                    j,
                                    "#[path] whose value is not a string literal".to_string(),
                                ),
                            });
                        }
                        _ => {}
                    }
                }
            }
            let Some(Token::Ident(name)) = tokens.get(i) else {
                continue;
            };
            if name == "mod" && punct(i + 1, '$') {
                references.push(unresolved(i, "a macro-generated mod $name".to_string()));
            }
            if name == "mod" {
                if let Some(Token::Ident(module)) = tokens.get(i + 1) {
                    let attributes = attributes(i);
                    let test_only = attributes.iter().any(is_cfg_test)
                        || inline.iter().any(|module| module.test_only);
                    if punct(i + 2, ';') {
                        if let Some(outer) = inline.iter().find(|module| module.path) {
                            references.push(unresolved(
                                i,
                                format!(
                                    "mod {module} inside the inline module {}, which has a #[path]",
                                    outer.name
                                ),
                            ));
                        } else {
                            references.push(Reference::Plain {
                                name: module.clone(),
                                within: inline
                                    .iter()
                                    .map(|module| module.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join("/"),
                                test_only,
                            });
                        }
                    } else if punct(i + 2, '{') {
                        inline.push(Inline {
                            name: module.clone(),
                            path: attributes.iter().any(names_path),
                            test_only,
                            end: group_end(i + 2),
                        });
                    }
                }
            }
            let include = matches!(name.as_str(), "include" | "include_str" | "include_bytes");
            if include && ident(i + 1, "as") {
                references.push(unresolved(i, format!("{name} renamed")));
            }
            if name == "debugger_visualizer" {
                references.push(unresolved(i, "debugger_visualizer".to_string()));
            }
            let close = match tokens.get(i + 2) {
                Some(Token::Punct('(')) => ')',
                Some(Token::Punct('[')) => ']',
                Some(Token::Punct('{')) => '}',
                _ => continue,
            };
            if include && punct(i + 1, '!') {
                references.push(match (tokens.get(i + 3), punct(i + 4, close)) {
                    (Some(Token::Str(target)), true) => Reference::Include(target.clone()),
                    _ => unresolved(i, format!("{name}! whose argument is not a string literal")),
                });
            }
        }
        references
    }

    #[test]
    fn rust_references_reads_path_modules_and_includes_and_skips_mentions() {
        let source = r##"
            #[path = "a.rs"] mod a;
            # [ path="b.rs" ] mod b;
            #[cfg_attr(test, path = "c.rs")] mod c;
            #[cfg_attr(feature = "path", path = "d.rs")]
            mod d;
            #[doc = "#[path = \"doc.rs\"]"] mod e;
            // #[path = "line-comment.rs"] mod f;
            /* #[path = "block.rs"] /* nested */ include_str!("block.json") */
            const QUOTE: char = '"';
            const ESCAPED: char = '\'';
            const BYTE: u8 = b'"';
            fn f<'a>(x: &'a str) -> &'a str { x }
            const S: &str = "include_str!(\"quoted.json\")";
            const R: &str = r#"include_str!("raw.json") "# ;
            const I: &str = include_str!( "e.json" );
            const J: &[u8] = include_bytes!(
                "f.bin"
            );
            include!("g.rs");
            const K: &str = include_str!(concat!("h", ".json"));
            #[r#path = "raw.rs"] mod raw;
            #[r#cfg_attr(all(), r#path = "raw-cfg.rs")] mod raw_cfg;
            #[path = MACRO] mod unreadable;
            const L: &str = include_str!{"brace.json"};
            const M: &[u8] = r#include_bytes!["bracket.bin"];
            fn g<'r>(x: &'r str) -> &'r str { x }
            mod plain;
            pub(crate) mod visible;
            mod inline { include!("inline.rs"); }
            mod outer { pub mod middle { mod nested; } mod sibling; }
            mod after;
            #[cfg(test)] mod tests;
            #[cfg(test)] #[allow(dead_code)] pub(crate) mod helpers;
            #[cfg(test)] mod test_block { mod fixture; }
            #[path = "dir"] mod pathed { mod lost; }
            mod pathed_inner { #[path = "z.rs"] mod z; }
            macro_rules! m { ($n:ident) => { mod $n; } }
            use std::include_str as grab;
            use std::{include_bytes as grab_bytes};
            #![debugger_visualizer(gdb_script_file = "g.py")]
        "##;
        use Reference::*;
        let plain = |name: &str| Plain {
            name: name.to_string(),
            within: String::new(),
            test_only: false,
        };
        let test_only = |name: &str| Plain {
            name: name.to_string(),
            within: String::new(),
            test_only: true,
        };
        let unresolved = |line: usize, form: &str| Unresolved {
            form: form.to_string(),
            line,
        };
        assert_eq!(
            rust_references(source),
            [
                Module("a.rs".to_string()),
                plain("a"),
                Module("b.rs".to_string()),
                plain("b"),
                Module("c.rs".to_string()),
                plain("c"),
                Module("d.rs".to_string()),
                plain("d"),
                plain("e"),
                Include("e.json".to_string()),
                Include("f.bin".to_string()),
                Include("g.rs".to_string()),
                unresolved(21, "include_str! whose argument is not a string literal"),
                Module("raw.rs".to_string()),
                plain("raw"),
                Module("raw-cfg.rs".to_string()),
                plain("raw_cfg"),
                unresolved(24, "#[path] whose value is not a string literal"),
                plain("unreadable"),
                Include("brace.json".to_string()),
                Include("bracket.bin".to_string()),
                plain("plain"),
                plain("visible"),
                Include("inline.rs".to_string()),
                Plain {
                    name: "nested".to_string(),
                    within: "outer/middle".to_string(),
                    test_only: false,
                },
                Plain {
                    name: "sibling".to_string(),
                    within: "outer".to_string(),
                    test_only: false,
                },
                plain("after"),
                test_only("tests"),
                test_only("helpers"),
                Plain {
                    name: "fixture".to_string(),
                    within: "test_block".to_string(),
                    test_only: true,
                },
                Module("dir".to_string()),
                unresolved(
                    36,
                    "mod lost inside the inline module pathed, which has a #[path]"
                ),
                unresolved(37, "#[path] inside the inline module pathed_inner"),
                Plain {
                    name: "z".to_string(),
                    within: "pathed_inner".to_string(),
                    test_only: false,
                },
                unresolved(38, "a macro-generated mod $name"),
                unresolved(39, "include_str renamed"),
                unresolved(40, "include_bytes renamed"),
                unresolved(41, "debugger_visualizer"),
            ]
        );
    }

    /// The `path = "..."` values of a Cargo manifest: the whole file for a
    /// `Cargo.toml`, and the `//!` ```` ```cargo ```` block for a rust-script.
    fn cargo_paths(rust_script: bool, source: &str) -> Vec<String> {
        let mut in_block = !rust_script;
        let mut paths = Vec::new();
        for line in source.lines() {
            let line = if rust_script {
                let Some(doc) = line.trim_start().strip_prefix("//!") else {
                    in_block = false;
                    continue;
                };
                let doc = doc.trim();
                if doc.starts_with("```") {
                    in_block = doc == "```cargo";
                    continue;
                }
                doc
            } else {
                line
            };
            if !in_block {
                continue;
            }
            let mut rest = line;
            while let Some(at) = rest.find("path") {
                let before = rest[..at].chars().next_back();
                rest = &rest[at + "path".len()..];
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                    continue;
                }
                let Some(value) = rest.trim_start().strip_prefix('=') else {
                    continue;
                };
                if let Some(value) = value.trim_start().strip_prefix('"') {
                    if let Some((value, _)) = value.split_once('"') {
                        paths.push(value.to_string());
                    }
                }
            }
        }
        paths
    }

    #[test]
    fn cargo_paths_reads_manifests_and_rust_script_blocks() {
        let script = "//! Docs: path = \"prose.rs\"\n\
                      //! ```cargo\n\
                      //! [dependencies]\n\
                      //! a = { path = \"../a\" }\n\
                      //! b = { version = \"1\", path=\"b\" }\n\
                      //! c = \"1\"\n\
                      //! ```\n\
                      //! ```text\n\
                      //! d = { path = \"not-cargo\" }\n\
                      //! ```\n\
                      const X: &str = \"path = \\\"code\\\"\";\n";
        assert_eq!(cargo_paths(true, script), ["../a", "b"]);
        let manifest = "[lib]\npath = \"src/lib.rs\"\n\
                        [dependencies]\n\
                        e = { path = \"../e\", subpath = \"x\" }\n";
        assert_eq!(cargo_paths(false, manifest), ["src/lib.rs", "../e"]);
    }

    /// A file's references and Cargo paths, read once per test process: the
    /// coverage test checks the same tree under several planted lists.
    struct Scanned {
        references: Vec<Reference>,
        cargo_paths: Vec<String>,
    }

    fn scanned(file: &Path) -> std::sync::Arc<Scanned> {
        type Cache = std::sync::Mutex<
            std::collections::HashMap<std::path::PathBuf, std::sync::Arc<Scanned>>,
        >;
        static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        if let Some(scanned) = cache.lock().unwrap().get(file) {
            return scanned.clone();
        }
        let source = std::fs::read_to_string(file).unwrap();
        let rust = file.extension().is_some_and(|extension| extension == "rs");
        let scanned = std::sync::Arc::new(Scanned {
            references: if rust {
                rust_references(&source)
            } else {
                Vec::new()
            },
            cargo_paths: cargo_paths(rust, &source),
        });
        cache
            .lock()
            .unwrap()
            .insert(file.to_path_buf(), scanned.clone());
        scanned
    }

    /// Every way `inputs` and `non_inputs` can drift from the tracked tree:
    /// an unclassified or doubly classified file under `roots`, a stale
    /// non-input, an input's `#[path]` module or plain `mod x;` file that no
    /// input covers, a file an input includes, or the file of a
    /// `#[cfg(test)]` plain module it declares, that is neither an input nor
    /// a non-input, a form whose file the scan cannot resolve, and a Cargo
    /// `path` in an input manifest (`Cargo.toml`, or a rust-script's
    /// `//!` cargo block) that no input or root covers. A non-input outside
    /// `roots` must be exactly an included file or a test-only module's.
    fn scorecard_input_violations(
        root: &Path,
        tracked: &[String],
        inputs: &[&str],
        roots: &[&str],
        non_inputs: &[(&str, &str)],
    ) -> Vec<String> {
        let mut violations = Vec::new();
        let is_input = |path: &str| inputs.iter().any(|input| is_under(path, input));
        let is_non_input = |path: &str| non_inputs.iter().any(|(entry, _)| is_under(path, entry));
        let mut included = BTreeSet::new();
        let mut reference_violations = Vec::new();
        for path in tracked
            .iter()
            .filter(|path| path.ends_with(".rs") && is_input(path))
        {
            let scanned = scanned(&root.join(path));
            let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            let stem = path.strip_suffix(".rs").unwrap_or(path);
            let mut seen = BTreeSet::new();
            for reference in &scanned.references {
                let (target, module) = match reference {
                    Reference::Module(target) => (target, true),
                    Reference::Include(target) => (target, false),
                    Reference::Unresolved { form, line } => {
                        reference_violations.push(format!(
                            "{path}:{line}: unresolved: {form}; the scan cannot classify its file"
                        ));
                        continue;
                    }
                    Reference::Plain {
                        name,
                        within,
                        test_only,
                    } => {
                        // Its file is `x.rs` or `x/mod.rs` beside a crate
                        // root or `mod.rs`, and under `<stem>/` otherwise,
                        // below the directories of the inline modules
                        // around it. A tracked candidate must be an input,
                        // or, for a test-only module (a `#[cfg(test)] mod
                        // tests;`), an input or a non-input; with none
                        // tracked, a `#[path]` names the file or nothing
                        // builds.
                        for base in [dir, stem] {
                            for candidate in [
                                format!("{base}/{within}/{name}.rs"),
                                format!("{base}/{within}/{name}/mod.rs"),
                            ] {
                                let candidate = normalize(&candidate);
                                if !tracked.contains(&candidate) || !seen.insert(candidate.clone())
                                {
                                    continue;
                                }
                                if !test_only && !is_input(&candidate) {
                                    reference_violations.push(format!(
                                        "{path} declares mod {name}, whose file {candidate} \
                                         no input covers"
                                    ));
                                } else if !is_input(&candidate) && !is_non_input(&candidate) {
                                    reference_violations.push(format!(
                                        "{path} declares mod {name}, whose file {candidate} \
                                         is neither an input nor a non-input"
                                    ));
                                }
                                included.insert(candidate);
                            }
                        }
                        continue;
                    }
                };
                let target = normalize(&format!("{dir}/{target}"));
                if !seen.insert(target.clone()) {
                    continue;
                }
                if !tracked.contains(&target) {
                    reference_violations
                        .push(format!("{path} includes {target}, which is not tracked"));
                } else if module && !is_input(&target) {
                    reference_violations
                        .push(format!("{path} includes {target}, which no input covers"));
                } else if !module && !is_input(&target) && !is_non_input(&target) {
                    reference_violations.push(format!(
                        "{path} includes {target}, which is neither an input nor a non-input"
                    ));
                }
                included.insert(target);
            }
        }
        // A Cargo `path` names a dependency's directory, or a target's file,
        // relative to its manifest.
        for path in tracked.iter().filter(|path| {
            (path.ends_with(".rs") || path.ends_with("Cargo.toml")) && is_input(path)
        }) {
            let scanned = scanned(&root.join(path));
            let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            for target in &scanned.cargo_paths {
                let target = normalize(&format!("{dir}/{target}"));
                let covered = if tracked.contains(&target) {
                    is_input(&target)
                } else {
                    let directory = format!("{target}/");
                    is_input(&directory) || roots.iter().any(|root| is_under(&directory, root))
                };
                if !covered {
                    reference_violations.push(format!(
                        "{path} gives Cargo the path {target}, which no input or input root covers"
                    ));
                }
            }
        }
        for (entry, reason) in non_inputs {
            if reason.trim().is_empty() {
                violations.push(format!("non-input {entry} has no reason"));
            }
            if !roots.iter().any(|dir| is_under(entry, dir)) && !included.contains(*entry) {
                violations.push(format!(
                    "non-input {entry} is outside the input roots and no input includes it"
                ));
            }
            if !tracked.iter().any(|path| is_under(path, entry)) {
                violations.push(format!("non-input {entry} names no tracked path"));
            }
        }
        for path in tracked {
            if !roots.iter().any(|dir| is_under(path, dir)) {
                continue;
            }
            match (is_input(path), is_non_input(path)) {
                (true, true) => violations.push(format!("{path} is both an input and a non-input")),
                (false, false) => {
                    violations.push(format!("{path} is neither an input nor a non-input"))
                }
                _ => {}
            }
        }
        violations.extend(reference_violations);
        violations
    }

    /// The scorecard's triggers cover everything its builds and runs read: a
    /// file under an input root must be classified, a `#[path]` module an
    /// input declares from anywhere in the tree must be an input, and a file
    /// an input includes, or the tracked file of a plain module it declares,
    /// must be classified (so the scan follows `scripts/validate.rs`'s module
    /// tree). Planted drifts show each check refuses.
    #[test]
    fn scorecard_inputs_cover_every_file_and_path_module() {
        use crate::validation_dag::SCORECARD_INPUT_ROOTS;
        use crate::validation_dag::SCORECARD_INPUTS;
        use crate::validation_dag::SCORECARD_NON_INPUTS;
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let tracked = tracked_paths(&root);
        let check = |inputs: &[&str], non_inputs: &[(&str, &str)]| {
            scorecard_input_violations(&root, &tracked, inputs, SCORECARD_INPUT_ROOTS, non_inputs)
        };
        assert_eq!(
            check(SCORECARD_INPUTS, SCORECARD_NON_INPUTS),
            Vec::<String>::new()
        );

        let without = |dropped: &str| -> Vec<&str> {
            let kept: Vec<&str> = SCORECARD_INPUTS
                .iter()
                .copied()
                .filter(|input| *input != dropped)
                .collect();
            assert_eq!(
                kept.len() + 1,
                SCORECARD_INPUTS.len(),
                "{dropped} is not an input"
            );
            kept
        };
        assert_eq!(
            check(
                &without("ci/record-replay-workloads.rs"),
                SCORECARD_NON_INPUTS
            ),
            [
                "ci/manifest-plan/src/nextest_binaries.rs includes ci/record-replay-workloads.rs, \
              which no input covers"
            ]
        );
        // The generator's modules are found through scripts/validate.rs, and a
        // module of a module through its parent.
        assert_eq!(
            check(
                &without("scripts/lib/validate_plan.rs"),
                SCORECARD_NON_INPUTS
            ),
            ["scripts/validate.rs includes scripts/lib/validate_plan.rs, which no input covers"]
        );
        assert_eq!(
            check(
                &without("scripts/lib/validate_nextest_fixture.rs"),
                SCORECARD_NON_INPUTS
            ),
            ["scripts/lib/validate_classification.rs includes \
                 scripts/lib/validate_nextest_fixture.rs, which no input covers"]
        );
        assert_eq!(
            check(
                &without("ci/manifest-plan/src/timeouts.rs"),
                SCORECARD_NON_INPUTS
            ),
            [
                "ci/manifest-plan/src/timeouts.rs is neither an input nor a non-input",
                "ci/manifest-plan/src/lib.rs declares mod timeouts, whose file \
                 ci/manifest-plan/src/timeouts.rs no input covers",
            ]
        );
        let without_non_input = |dropped: &str| -> Vec<(&str, &str)> {
            let kept: Vec<(&str, &str)> = SCORECARD_NON_INPUTS
                .iter()
                .copied()
                .filter(|(entry, _)| *entry != dropped)
                .collect();
            assert_eq!(kept.len() + 1, SCORECARD_NON_INPUTS.len());
            kept
        };
        assert_eq!(
            check(
                SCORECARD_INPUTS,
                &without_non_input("ci/portable-shards.json")
            ),
            [
                "ci/manifest-plan/src/bin/test-harness.rs includes ci/portable-shards.json, \
                 which is neither an input nor a non-input"
            ]
        );
        assert_eq!(
            check(
                SCORECARD_INPUTS,
                &without_non_input("tests/fixtures/scorecard-writeback/refusal.txt")
            ),
            [
                "scripts/validate.rs includes tests/fixtures/scorecard-writeback/refusal.txt, \
                 which is neither an input nor a non-input"
            ]
        );
        // A plain `mod admission;` in ledger.rs, which is not a crate root,
        // names ledger/admission.rs.
        assert_eq!(
            check(
                &without("ci/manifest-plan/src/ledger/admission.rs"),
                SCORECARD_NON_INPUTS
            ),
            [
                "ci/manifest-plan/src/ledger/admission.rs is neither an input nor a non-input",
                "ci/manifest-plan/src/ledger.rs declares mod admission, whose file \
                 ci/manifest-plan/src/ledger/admission.rs no input covers",
            ]
        );
        // A module that is not test-only must be an input: listing it as a
        // non-input does not satisfy the check.
        let mut moved = SCORECARD_NON_INPUTS.to_vec();
        moved.push(("ci/manifest-plan/src/timeouts.rs", "planted"));
        assert_eq!(
            check(&without("ci/manifest-plan/src/timeouts.rs"), &moved),
            [
                "ci/manifest-plan/src/lib.rs declares mod timeouts, whose file \
              ci/manifest-plan/src/timeouts.rs no input covers"
            ]
        );
        let mut doubled = SCORECARD_NON_INPUTS.to_vec();
        doubled.push(("ci/compat-envelope/cells.json", "planted"));
        doubled.push(("ci/compat-envelope/gone.json", ""));
        doubled.push(("README.md", "planted"));
        // Only the included file itself, not a directory holding it.
        doubled.push(("tests/fixtures/scorecard-writeback/", "planted"));
        assert_eq!(
            check(SCORECARD_INPUTS, &doubled),
            [
                "non-input ci/compat-envelope/gone.json has no reason",
                "non-input ci/compat-envelope/gone.json names no tracked path",
                "non-input README.md is outside the input roots and no input includes it",
                "non-input tests/fixtures/scorecard-writeback/ is outside the input roots \
                 and no input includes it",
                "ci/compat-envelope/cells.json is both an input and a non-input",
            ]
        );
    }

    /// On a planted tree: a plain module inside an inline module is found
    /// under the inline module's directory, a test-only module may be a
    /// non-input and any other must be an input, a form the scan cannot
    /// resolve is refused, and a rust-script's Cargo path must be covered.
    #[test]
    fn scorecard_input_violations_follow_inline_modules_and_refuse_the_rest() {
        let scratch = Scratch::new("input-violations");
        let files = [
            (
                "s/main.rs",
                "//! ```cargo\n//! [dependencies]\n//! dep = { path = \"../dep\" }\n//! ```\n\
                 mod inner { mod x; }\n\
                 #[cfg(test)]\nmod tests;\n\
                 mod helper;\n\
                 use std::include_str as grab;\n",
            ),
            ("s/inner/x.rs", ""),
            ("s/x.rs", ""),
            ("s/tests.rs", ""),
            ("s/helper.rs", ""),
            ("dep/Cargo.toml", ""),
        ];
        for (path, contents) in files {
            let path = scratch.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let tracked: Vec<String> = files.iter().map(|(path, _)| path.to_string()).collect();
        let non_inputs = [("s/tests.rs", "test-only"), ("s/helper.rs", "planted")];
        assert_eq!(
            scorecard_input_violations(&scratch.0, &tracked, &["s/main.rs"], &["r/"], &non_inputs),
            [
                "s/main.rs declares mod x, whose file s/inner/x.rs no input covers",
                "s/main.rs declares mod helper, whose file s/helper.rs no input covers",
                "s/main.rs:9: unresolved: include_str renamed; the scan cannot classify its file",
                "s/main.rs gives Cargo the path dep, which no input or input root covers",
            ]
        );
        assert_eq!(
            scorecard_input_violations(
                &scratch.0,
                &tracked,
                &["s/main.rs", "s/inner/x.rs", "s/helper.rs", "dep/"],
                &["r/"],
                &non_inputs[..1],
            ),
            ["s/main.rs:9: unresolved: include_str renamed; the scan cannot classify its file"]
        );
    }

    /// A scratch repository for `change_set`, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "hermit-self-test-selection-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn git(&self, args: &[&str]) -> String {
            let output = git_command()
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args([
                    "-c",
                    "user.name=fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                ])
                .arg("-C")
                .arg(&self.0)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        }

        fn commit(&self, path: &str) -> String {
            let file = self.0.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, path).unwrap();
            self.git(&["add", "--", path]);
            self.git(&["commit", "-qm", path]);
            self.git(&["rev-parse", "HEAD"])
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn change_set_reads_the_branch_diff_and_untracked_paths_and_runs_main() {
        let repo = Scratch::new("branch");
        repo.git(&["init", "-q", "-b", "main"]);
        let base = repo.commit("README.md");
        repo.git(&["update-ref", "refs/remotes/origin/main", &base]);
        repo.git(&["switch", "-q", "-c", "topic"]);
        repo.commit("docs/one.md");
        std::fs::create_dir_all(repo.0.join("ci/compat-envelope")).unwrap();
        std::fs::write(repo.0.join("ci/compat-envelope/new.rs"), "untracked").unwrap();
        assert_eq!(
            change_set(&repo.0),
            Ok((
                base.clone(),
                vec![
                    "ci/compat-envelope/new.rs".to_string(),
                    "docs/one.md".to_string()
                ]
            ))
        );
        // Without the untracked file the branch touches no trigger.
        std::fs::remove_file(repo.0.join("ci/compat-envelope/new.rs")).unwrap();
        assert!(matches!(
            decide(None, change_set(&repo.0), TRIGGERS),
            Decision::Skip { base: skipped, .. } if skipped == base
        ));

        // A commit already contained in main runs, through the local branch
        // or through any remote's main.
        repo.git(&["switch", "-q", "main"]);
        let on_main = change_set(&repo.0).unwrap_err();
        assert!(
            on_main.contains("HEAD is on main (refs/heads/main"),
            "{on_main}"
        );
        repo.git(&["switch", "-q", "topic"]);
        let topic = repo.git(&["rev-parse", "HEAD"]);
        repo.git(&["branch", "-q", "-D", "main"]);
        repo.git(&["update-ref", "refs/remotes/upstream/main", &topic]);
        let remote_main = change_set(&repo.0).unwrap_err();
        assert!(
            remote_main.contains("HEAD is on main (refs/remotes/upstream/main)"),
            "{remote_main}"
        );
        repo.git(&["update-ref", "-d", "refs/remotes/upstream/main"]);

        // No origin/main to take a merge base from runs.
        repo.git(&["update-ref", "-d", "refs/remotes/origin/main"]);
        let no_base = change_set(&repo.0).unwrap_err();
        assert!(no_base.contains("git merge-base"), "{no_base}");
    }

    #[test]
    fn change_set_lists_deleted_renamed_and_submodule_paths() {
        let repo = Scratch::new("moves");
        repo.git(&["init", "-q", "-b", "main"]);
        repo.commit("ci/compat-envelope/old.rs");
        repo.commit("ci/compat-envelope/gone.rs");
        // An unpopulated submodule, as a clone without `--recurse-submodules`
        // leaves it: an empty directory.
        std::fs::create_dir_all(repo.0.join("agent-utils")).unwrap();
        let gitlink = |commit: &str| {
            repo.git(&[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{commit},agent-utils"),
            ]);
        };
        gitlink(&"1".repeat(40));
        repo.git(&["commit", "-qm", "submodule"]);
        let base = repo.git(&["rev-parse", "HEAD"]);
        repo.git(&["update-ref", "refs/remotes/origin/main", &base]);
        repo.git(&["switch", "-q", "-c", "topic"]);
        repo.git(&["mv", "ci/compat-envelope/old.rs", "docs-old.rs"]);
        repo.git(&["rm", "-q", "ci/compat-envelope/gone.rs"]);
        repo.git(&["commit", "-qm", "move"]);
        let (_, moved) = change_set(&repo.0).unwrap();
        assert_eq!(
            moved,
            vec![
                "ci/compat-envelope/gone.rs".to_string(),
                "ci/compat-envelope/old.rs".to_string(),
                "docs-old.rs".to_string(),
            ]
        );

        // A moved submodule commit is listed even when the clone asks diff to
        // ignore submodules.
        repo.git(&["reset", "-q", "--hard", &base]);
        repo.git(&["config", "diff.ignoreSubmodules", "all"]);
        gitlink(&"2".repeat(40));
        repo.git(&["commit", "-qm", "bump"]);
        assert_eq!(
            change_set(&repo.0),
            Ok((base, vec!["agent-utils".to_string()]))
        );
    }

    #[test]
    fn change_set_runs_in_a_shallow_clone() {
        let source = Scratch::new("shallow-source");
        source.git(&["init", "-q", "-b", "main"]);
        source.commit("README.md");
        source.commit("docs/one.md");
        let clone = Scratch::new("shallow-clone");
        let url = format!("file://{}", source.0.display());
        source.git(&[
            "clone",
            "-q",
            "--depth",
            "1",
            "--no-local",
            &url,
            clone.0.to_str().unwrap(),
        ]);
        clone.git(&["switch", "-q", "-c", "topic"]);
        clone.git(&["branch", "-q", "-D", "main"]);
        clone.git(&["update-ref", "-d", "refs/remotes/origin/main"]);
        let shallow = change_set(&clone.0).unwrap_err();
        assert!(shallow.contains("this clone is shallow"), "{shallow}");
    }

    #[test]
    fn anything_unresolved_or_forced_runs() {
        for decision in [
            decide(None, Err("this clone is shallow".into()), TRIGGERS),
            decide(None, change(&[]), TRIGGERS),
            decide(Some("all"), change(&["detcore/src/lib.rs"]), TRIGGERS),
            decide(Some("some"), change(&["detcore/src/lib.rs"]), TRIGGERS),
        ] {
            assert!(matches!(decision, Decision::Run(_)), "{decision:?}");
        }
    }
}
