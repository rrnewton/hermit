#!/usr/bin/env -S rust-script --force
//! Refuse a checker that nothing schedules.
//!
//! THE GAP THIS CLOSES. `check.lint_checks` made a checker added to the Makefile's
//! `lint-checks` target gated by construction, which removed "someone must also
//! hand-write a DAG node" as a failure mode. It did NOT cover the other shape: a
//! checker that is added to no target at all. Measured 2026-08-25 at main
//! a5fef7ff7623, four checkers were reachable from nothing whatsoever --
//! ci/check-derived-current-after-merge.sh was referenced by ZERO files in the
//! repository, and check-detcore-backend-abstraction-test.sh was named only inside
//! a comment. A checker nothing runs is indistinguishable from a checker that
//! passes, so it is worse than absent: it reads as coverage.
//!
//! WHAT IT ASSERTS. Every tracked, executable checker entrypoint is reachable from
//! either a `cmd` in ci/dag/*.json or the Makefile's `lint-checks` recipe, directly
//! or through another reachable script. Anything else must be listed in ALLOWLIST
//! with a reason, so unscheduled checkers are visible debt rather than silence.
//!
//! ⚠️ TWO TRAPS THIS DELIBERATELY AVOIDS, both of which caught the manual sweep
//! that produced it:
//!
//!   1. A reference inside a COMMENT is not an invocation.
//!      scripts/check-detcore-backend-abstraction.sh:264 names
//!      check-detcore-backend-abstraction-test.sh in a comment, and a plain grep
//!      reports it as scheduled. Comments are stripped before matching.
//!   2. A reference in .github/workflows/ is not evidence of anything. All ten
//!      workflows are workflow_dispatch-only except linux-boot.yml, so they gate
//!      no PR. Workflows are NOT a reachability source here, by design.
//!
//! Reachability is a fixpoint, not one hop: check-git-pin-uniformity.rs has no DAG
//! node but ci/run-reverie-pin-check.sh calls it and that has two, so it IS
//! scheduled. Stopping at one hop would report it as an orphan.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

/// Checkers that are deliberately NOT scheduled, each with the reason.
///
/// This is a RATCHET, not an amnesty. Every entry is a real gap that someone
/// decided not to close today; adding to it is a deliberate, reviewed act, and the
/// list should shrink. It exists so this checker starts green on a tree that
/// already has four orphans rather than landing as an immediate red.
const ALLOWLIST: &[(&str, &str)] = &[
    (
        "scripts/check-default-build-warnings.sh",
        "Builds the workspace; its findings were never counted, so wiring it \
         untriaged could convert a silent gap into a standing red. Only \
         scripts/setup-hooks.sh references it, and only to chmod +x. Run it on a \
         quiet box, count what it finds, then schedule or delete it.",
    ),
    (
        "scripts/test-fail-closed.sh",
        "Builds the workspace; findings never counted, same reasoning as \
         check-default-build-warnings.sh. Reached only from \
         scripts/progress-report.sh, which is itself unscheduled.",
    ),
    (
        "ci/check-derived-current-after-merge.sh",
        "Referenced by ZERO files in the repository. Passes when run by hand \
         (measured 2026-08-25). Scheduling it is a judgement about whether it is \
         live or dead code, which should be made explicitly rather than by this \
         guard's default.",
    ),
    (
        "scripts/check-detcore-backend-abstraction-test.sh",
        "Named only inside a comment at check-detcore-backend-abstraction.sh:264. \
         Passes when run by hand. It looks like the self-test arm of that checker \
         and most likely wants to be invoked by it; that is a change to that \
         checker, not to this list.",
    ),
    (
        "scripts/test_validate_stop_paths.py",
        "CANNOT be scheduled: it SPAWNS A VALIDATE. Its run_signal does \
         Popen([scripts/validate.rs, 'full']), so as a DAG node it runs a validate from \
         inside a validate; the child is refused and exits 2 before emitting \
         VALIDATE_STOP_TEST_READY. Measured on main at 7406b4dd2efc, deterministically, \
         after the other ten checkers passed. It passes STANDALONE, which is why wiring \
         it looked safe -- a checker that spawns the harness it runs under is not merely \
         unscheduled, it is unschedulable in this shape. Run it by hand, or give it a \
         mode that does not launch a real validate.",
    ),
    (
        "ci/check-shard-coverage.sh",
        "Unscheduled, and a WORKING guard: it caught a real error in the change \
         that added check.lint_checks (a node assigned to no shard). Its only \
         reference is ci-portable.yml, which is workflow_dispatch-only. It should \
         be scheduled; doing so belongs in its own head with its own count.",
    ),
];

/// Directory/prefix pairs that identify a checker entrypoint by convention.
const CHECKER_PREFIXES: &[&str] = &[
    "scripts/check-",
    "scripts/test-",
    "scripts/test_",
    "ci/check-",
    "ci/verify-",
    "ci/test-",
];

/// This checker's own tracked path.
const SELF_PATH: &str = "scripts/check-checker-scheduling.rs";

/// Whether a reached script's BODY may be read as a source of invocations.
///
/// ⚠️ THIS FILE IS REACHED BUT IS NOT AN INVOCATION SOURCE, AND THE DISTINCTION IS
/// LOAD-BEARING. `ALLOWLIST` names every allowlisted checker as a string literal,
/// and each of those entries is a DECLARATION THAT THE PATH IS NOT SCHEDULED --
/// the exact opposite of an invocation. Feeding this file's text back into the
/// fixpoint therefore marks every allowlisted checker as reached and reports it
/// as a stale allowlist entry.
///
/// It is self-defeating rather than merely wrong: the bug appears only once this
/// checker is itself added to the `lint-checks` recipe, which is what makes it
/// reachable, which is what makes it read its own body. Measured 2026-08-25 on
/// this branch rebased onto main: with the recipe line present, 5 of 5 allowlist
/// entries reported STALE and `make lint-checks` exited 1; with the line removed
/// and nothing else changed, 0 did. A gate that cannot be scheduled without
/// breaking itself is not a gate.
///
/// The exclusion is by path rather than by content: a heuristic that tried to
/// tell a declaration from an invocation inside arbitrary Rust would be the kind
/// of guess this checker exists to avoid.
fn expands_frontier(path: &str) -> bool {
    path != SELF_PATH
}

fn tracked_files() -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files"])
        .output()
        .expect("git ls-files failed to spawn");
    assert!(out.status.success(), "git ls-files exited nonzero");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Strip comment text so a mention inside a comment is not read as an invocation.
///
/// Deliberately conservative: only whole-line comments are removed. A trailing
/// `# ...` on a live command line is rare in these files and dropping it risks
/// discarding a real invocation, which would produce a FALSE ORPHAN -- the
/// expensive direction of error here.
fn strip_comments(text: &str, path: &str) -> String {
    let rust_like = path.ends_with(".rs");
    text.lines()
        .filter(|line| {
            let t = line.trim_start();
            if t.is_empty() {
                return false;
            }
            if rust_like {
                !(t.starts_with("//") || t.starts_with("/*") || t.starts_with('*'))
            } else {
                !t.starts_with('#')
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_executable(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn main() {
    rust_script_prelude::init();
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "check-checker-scheduling: refuse a tracked checker entrypoint that no DAG\n\
             node and no `make lint-checks` recipe line can reach, directly or\n\
             transitively. Workflows are not a reachability source: they are\n\
             workflow_dispatch-only and gate nothing. Deliberate exceptions live in\n\
             ALLOWLIST with a reason each."
        );
        return;
    }
    if std::env::args().any(|a| a == "--self-test") {
        self_test();
        return;
    }

    let tracked = tracked_files();

    // Every checker entrypoint we expect to be scheduled.
    let checkers: BTreeSet<String> = tracked
        .iter()
        .filter(|p| CHECKER_PREFIXES.iter().any(|pre| p.starts_with(pre)))
        .filter(|p| p.ends_with(".sh") || p.ends_with(".rs") || p.ends_with(".py"))
        .filter(|p| is_executable(p) || p.ends_with(".py"))
        .cloned()
        .collect();
    assert!(
        !checkers.is_empty(),
        "discovered no checker entrypoints; the naming convention or this filter moved"
    );
    // A rename must not silently re-enable the self-reference described on
    // `expands_frontier`. If SELF_PATH stops naming a tracked file the exclusion
    // matches nothing, every allowlist entry reads as stale again, and the only
    // symptom is a confident wrong answer.
    assert!(
        tracked.iter().any(|p| p == SELF_PATH),
        "SELF_PATH ({SELF_PATH}) is not tracked; update it to this file's path"
    );

    // Seed the reachable set from the two things that actually schedule work.
    //
    // ⚠️ ONLY THE `cmd` FIELDS, never the whole JSON. A DAG node's `description` is
    // prose and routinely NAMES checkers it does not run -- check.lint_checks's own
    // description lists six of them. Seeding from the raw file therefore counted a
    // checker mentioned in prose as scheduled, which is the false negative this guard
    // exists to prevent: it would report an orphan as covered because someone wrote
    // about it. Caught 2026-08-25 when a freshly ALLOWLISTED entry was reported STALE
    // ("now scheduled") purely because a node description mentioned it.
    //
    // Same defect class as counting a marker's TEXT instead of the binding: the
    // instrument must read the field that CAUSES execution, not the field that
    // discusses it.
    let mut seed = String::new();
    for dag in ["ci/dag/portable.json", "ci/dag/privileged.json"] {
        if Path::new(dag).exists() {
            let raw = std::fs::read_to_string(dag).unwrap();
            let cmds = extract_cmds(&raw);
            assert!(
                !cmds.is_empty(),
                "{dag} yielded no `cmd` fields; the schema moved and this guard would \
                 otherwise report every checker as an orphan"
            );
            for c in cmds {
                seed.push_str(&c);
                seed.push('\n');
            }
        }
    }
    seed.push_str(&lint_checks_recipe(
        &std::fs::read_to_string("Makefile").expect("Makefile is unreadable"),
    ));

    // Fixpoint over EVERY tracked script, not only the checkers.
    //
    // ⚠️ Expanding only checkers is wrong and the first version of this guard did
    // it. The intermediate scripts are what carry reachability: ci/run-reverie-pin-
    // check.sh has two DAG nodes and calls both check-reverie-pin.rs and
    // check-git-pin-uniformity.rs, and scripts/validate.rs calls
    // ci/verify-hermit-e2e-artifact.sh. Neither intermediary matches a checker
    // prefix, so a checkers-only frontier stopped dead at the DAG and reported all
    // three as orphans. False orphans are the expensive direction: they train a
    // reader to allowlist a checker that was scheduled all along.
    let all_scripts: BTreeSet<String> = tracked
        .iter()
        .filter(|p| p.ends_with(".sh") || p.ends_with(".rs") || p.ends_with(".py"))
        .filter(|p| !p.starts_with("third-party/"))
        .cloned()
        .collect();

    let mut reached: BTreeSet<String> = BTreeSet::new();
    let mut frontier = vec![seed];
    while let Some(text) = frontier.pop() {
        for s in &all_scripts {
            if reached.contains(s) {
                continue;
            }
            if is_invoked(&text, s) {
                reached.insert(s.clone());
                if expands_frontier(s) {
                    if let Ok(body) = std::fs::read_to_string(s) {
                        frontier.push(strip_comments(&body, s));
                    }
                }
            }
        }
    }

    let allow: BTreeMap<&str, &str> = ALLOWLIST.iter().copied().collect();
    let mut unscheduled: Vec<&String> = checkers
        .iter()
        .filter(|c| !reached.contains(*c))
        .filter(|c| !allow.contains_key(c.as_str()))
        .collect();
    unscheduled.sort();

    // A stale allowlist is its own defect: an entry that IS now scheduled, or names
    // a file that no longer exists, quietly grants permission nobody needs.
    let mut stale: Vec<&str> = ALLOWLIST
        .iter()
        .map(|(p, _)| *p)
        .filter(|p| reached.contains(*p) || !Path::new(p).exists())
        .collect();
    stale.sort();

    if unscheduled.is_empty() && stale.is_empty() {
        println!(
            "check-checker-scheduling: OK -- {} checker entrypoint(s), {} scheduled, \
             {} allowlisted with reasons.",
            checkers.len(),
            reached.iter().filter(|r| checkers.contains(*r)).count(),
            ALLOWLIST.len()
        );
        return;
    }

    if !unscheduled.is_empty() {
        eprintln!(
            "check-checker-scheduling: {} checker(s) are scheduled by NOTHING.",
            unscheduled.len()
        );
        eprintln!(
            "  A checker nothing runs is indistinguishable from a checker that passes."
        );
        eprintln!("  Add it to the Makefile's `lint-checks` recipe (which check.lint_checks");
        eprintln!("  runs, so no DAG edit is needed), or to a DAG node, or add it to");
        eprintln!("  ALLOWLIST in this file WITH A REASON.");
        for c in &unscheduled {
            eprintln!("    {c}");
        }
    }
    for p in &stale {
        eprintln!("check-checker-scheduling: STALE ALLOWLIST entry {p} -- now scheduled or absent; remove it.");
    }
    std::process::exit(1);
}

/// A runner, then any number of FLAGS, then the path: `rustc --edition=2021 <path>`.
///
/// Tokenised per line rather than matched as a substring, so `--edition=2021` between
/// the runner and its input does not hide the invocation. Stops at the first token that
/// is not a flag; see the note at the call site for why it deliberately does not scan
/// further.
fn runner_flag_path(text: &str, path: &str) -> bool {
    const RUNNERS: [&str; 5] = ["python3", "python", "bash", "sh", "rustc"];
    /// Flags whose VALUE is the next token, so that token is never the script.
    ///
    /// ⚠️ THE ORIGINAL NOTE HERE HAD THE RISK BACKWARDS, and that is why this list
    /// exists rather than being avoided. It said naming these "is a guess -- and a
    /// wrong guess here fails in the silent direction". Both ways of being wrong
    /// were checked, and neither does:
    ///
    ///   * OVER-marking (listing a flag that takes no value): the scan skips one
    ///     token too many, lands past the script, and reports a FALSE ORPHAN --
    ///     loud, and one line to fix.
    ///   * UNDER-marking (the state before this list): the scan stops ON the
    ///     value, so `rustc -o <checker> in.rs` reads an OUTPUT as an invocation
    ///     and reports a FALSE "SCHEDULED" -- silent, and it defeats the guard.
    ///
    /// So omitting the list is the silent failure and having it is the loud one.
    /// Both directions are pinned in `self_test`.
    /// Flags known NOT to take a separate value.
    ///
    /// WARNING -- THE DEFAULT IS INVERTED FROM WHAT IT WAS, AND THE INVERSION IS THE
    /// FIX. This was a `VALUE_FLAGS` table listing flags that DO consume the next
    /// token, everything unlisted assumed not to. That puts the burden on the table
    /// being complete, and an incomplete table fails SILENTLY: the scan stops on a
    /// flag's value and reads it as the script, reporting a false SCHEDULED. Three
    /// heads paid for that in one night -- `-o` (hermit#2667), `-c` (hermit#2674),
    /// and `-X`, still live after both landed.
    ///
    /// Now an unrecognised flag is ASSUMED to consume the token after it. Getting
    /// that wrong steps past the script and reports a false ORPHAN -- loud, one line
    /// to fix. A missing entry here can no longer hide a checker, which is the
    /// property the old table could not have at any length.
    ///
    /// Attached values need no entry: `--edition=2021`, `-Dwarnings`.
    const NO_VALUE_FLAGS: [&str; 12] = [
        // python
        "-B", "-E", "-I", "-O", "-OO", "-s", "-S", "-u", "-v", "-b",
        // shells
        "-e", "-x",
    ];
    let dotted = format!("./{path}");
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        for (index, token) in tokens.iter().enumerate() {
            if !RUNNERS.contains(token) {
                continue;
            }
            let mut next = index + 1;
            while next < tokens.len() && tokens[next].starts_with('-') {
                let flag = tokens[next];
                // Attached value: one token. Known boolean: consumes nothing.
                // EVERYTHING ELSE IS ASSUMED to consume the token after it --
                // see NO_VALUE_FLAGS for why that default and not the other.
                let attached =
                    flag.contains('=') || (!flag.starts_with("--") && flag.len() > 2);
                if !attached && !NO_VALUE_FLAGS.contains(&flag) {
                    next += 1;
                }
                next += 1;
            }
            if next < tokens.len() && (tokens[next] == path || tokens[next] == dotted) {
                return true;
            }
        }
    }
    false
}

/// Does `text` INVOKE `path`, as opposed to merely naming it?
///
/// ⚠️ A bare substring match on the basename is not good enough, and this is the
/// second time that exact shortcut produced a wrong answer here. A filename appears
/// in plenty of live, non-comment text that does not run it -- most sharply, an
/// `echo` inside ci/lint-checks-node.sh names test_validate_stop_paths.py in a
/// diagnostic message, which read as an invocation and silently marked an
/// unschedulable checker as scheduled.
///
/// So require one of the shapes this repository actually uses to RUN a script:
///
///     ./scripts/foo.sh            $(SUBMODULE_PROXY) ./ci/foo.sh
///     python3 ./scripts/foo.py    python3 scripts/foo.py
///     bash scripts/foo.sh         sh scripts/foo.sh
///
/// The bare-path forms are deliberately restricted to an explicit interpreter prefix.
/// Erring toward a FALSE ORPHAN is the safe direction: it is loud and a human fixes
/// it in one line, whereas a false "scheduled" is silent and defeats the guard.
fn is_invoked(text: &str, path: &str) -> bool {
    // WARNING -- EVERY CLAUSE HERE GOES THROUGH `invokes_on_line`, AND THAT IS THE
    // SHAPE THAT MATTERS. This was a disjunction in which only the runner clause
    // consulted a guard and this one was a bare `text.contains`. Because the clauses
    // are OR-ed the WEAKEST decided the answer, so every hardening added to the flag
    // scan -- hermit#2661, #2667, #2674 -- was reachable only when this clause had
    // already declined. Two characters defeated all three: `rustc -o ./scripts/x.rs`
    // read as an invocation. A guard one arm of an OR can bypass is not a guard.
    let dotted = format!("./{path}");
    if text.lines().any(|line| invokes_on_line(line, &dotted)) {
        return true;
    }
    // `rustc` is a runner here too: ci/run-reverie-pin-check.sh COMPILES
    // scripts/check-reverie-pin.rs and runs the resulting binary, so the checker is
    // genuinely scheduled without ever being executed as a script.
    for runner in ["python3 ", "python ", "bash ", "sh ", "rustc "] {
        let needle = format!("{runner}{path}");
        // ⚠️ AND THE RUNNER PREFIX MUST NOT BE INSIDE A STRING LITERAL. Requiring
        // `python3 ` in front of the path was still not enough: `ci/lint-checks-node.sh`
        // carries `"python3 scripts/test_validate_stop_paths.py` as FIXTURE DATA -- a
        // quoted multi-line string handed to `check_run` to simulate make's output --
        // and that read as an invocation. Measured 2026-08-26: it turned
        // check-checker-scheduling RED on main immediately after #2622 merged, because
        // the entry #2622 correctly allowlisted was then reported STALE ("now
        // scheduled") on the strength of a test fixture.
        //
        // A real invocation begins a command: the runner is at the start of a line, or
        // follows a shell operator. It is never preceded by a quote. This is the THIRD
        // narrowing of this predicate, and each one has been the same lesson -- a
        // filename in live code is not evidence that the code runs it.
        if text.lines().any(|line| invokes_on_line(line, &needle)) {
            return true;
        }
    }
    // ⚠️ A FLAG MAY SIT BETWEEN THE RUNNER AND THE PATH, and requiring them adjacent
    // reported a genuinely scheduled checker as an orphan. Found by agent(hermit-004)
    // wiring check.exit_status_class: `rustc --edition=2021 scripts/check-exit-status-class.rs`
    // matched nothing -- the cmd has no `./<path>` and no line STARTS with the path,
    // so every clause missed and the node was called scheduled by NOTHING.
    //
    // ⚠️ ONLY LEADING FLAGS ARE SKIPPED, AND THE SCAN STOPS AT THE FIRST NON-FLAG TOKEN.
    // It does NOT search the whole line for the path, because `rustc a.rs -o <path>`
    // would then read an OUTPUT as an invocation -- a false "scheduled", which is silent
    // and defeats the guard. Erring to a FALSE ORPHAN is the direction this file already
    // chose: loud, and a human fixes it in one line.
    //
    // The known cost of stopping at the first non-flag token: `rustc -o out <path>`, where
    // a flag takes a SEPARATE value, still misses. Skipping that would require knowing
    // which flags consume the next token, which is a guess -- and a wrong guess here fails
    // in the silent direction.
    if runner_flag_path(text, path) {
        return true;
    }
    // A path alone at the start of a line is a shell line-continuation argument --
    // the form run-reverie-pin-check.sh uses for both pin checkers. Anchoring to the
    // line start is what keeps this from matching a filename quoted mid-sentence.
    text.lines().any(|line| invokes_on_line(line, path))
}

/// Is `needle` used as a COMMAND on this line, rather than quoted inside a string?
///
/// The discriminator is the character immediately before the match: a command starts
/// a line or follows a shell operator (`;`, `&`, `|`, `(`, backtick) or `$(`. A quote
/// before it means the text is data -- an echoed diagnostic, or a fixture standing in
/// for output. Erring toward FALSE ORPHAN stays the safe direction.
fn invokes_on_line(line: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(hit) = line[from..].find(needle) {
        let at = from + hit;
        if in_command_position(&line[..at]) {
            return true;
        }
        from = at + 1;
    }
    false
}

/// Is a match starting right after `before` THE FIRST WORD OF A COMMAND?
///
/// WARNING -- THIS USED TO ASK WHICH CHARACTER PRECEDED THE MATCH, which is the same
/// mistake as every other defect in this file: a semantic question answered by
/// enumerating syntax. The old rule accepted a match preceded by nothing, `;`, `&`,
/// `|`, `(` or a backtick. Two real invocations here are preceded by neither --
/// `$(SUBMODULE_PROXY) ./scripts/check-nested-lockfiles.rs` in the Makefile, and
/// `VAR=value ./ci/node.sh` in a DAG command -- so funnelling the other clauses
/// through the old guard reported 2 checkers as scheduled by NOTHING. Adding `)` and
/// `=` to the character list would have fixed those two and left the third shape to
/// be found later.
fn in_command_position(before: &str) -> bool {
    // The match must START A WORD. `OUT=./scripts/check-x.sh` is an assignment's
    // VALUE, not a command, and the match there is contiguous with the token before
    // it. This also rejects a shell-quoted match, since a quote is not whitespace.
    let mut before = before;
    // A quote directly before the match may be a string DELIMITER rather than shell
    // quoting -- DAG commands reach this file as shell text, as JSON (decoded
    // upstream) and as RUST string literals in scripts/validate.rs, which sets
    // `node.cmd = "./ci/verify-hermit-e2e-artifact.sh ..."`. Strip one such quote and
    // judge what precedes IT; the word test below then rejects the cases where the
    // quote was not opening anything, so no separate rule is needed for that.
    if let Some(rest) = before.strip_suffix('"').or_else(|| before.strip_suffix('\'')) {
        before = rest;
    }
    // The match must START A WORD. `OUT=./scripts/check-x.sh` is an assignment's
    // VALUE, not a command: the match is contiguous with the token before it.
    if let Some(last) = before.chars().last() {
        if !last.is_whitespace() && !matches!(last, ';' | '&' | '|' | '(' | '`') {
            return false;
        }
    }
    let trimmed = before.trim_end();
    if trimmed.is_empty() {
        return true;
    }
    // Walk back: everything between the match and a command boundary must be a thing
    // that PRECEDES a command rather than being one.
    for token in trimmed.split_whitespace().rev() {
        if is_command_boundary(token) {
            return true;
        }
        if is_assignment(token) || is_expansion(token) {
            continue;
        }
        return false;
    }
    true
}

/// `NAME=value` -- an environment prefix, which precedes a command.
fn is_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `$(VAR)`, `${VAR}` or `$VAR` -- may expand to a command prefix.
fn is_expansion(token: &str) -> bool {
    token.starts_with('$')
}

/// A token that ENDS the previous command, so what follows begins a new one.
fn is_command_boundary(token: &str) -> bool {
    // A BARE `=` is an assignment operator with spaces around it, which shell
    // assignments never have (`VAR=value` is one token). It only occurs in source
    // that BUILDS a command string -- `node.cmd = "./ci/x.sh"` in validate.rs -- so
    // accepting it opens no shell shape, and the value it introduces starts fresh.
    matches!(token, ";" | "&&" | "||" | "|" | "&" | "(" | "`" | "=")
        || token.ends_with(';')
        || token.ends_with("&&")
        || token.ends_with("||")
        || token.ends_with('|')
        || token.ends_with('`')
}
/// Decode a JSON string literal into the text a shell would see.
///
/// WARNING -- WITHOUT THIS THE QUOTE GUARD IS MEANINGLESS ON DAG COMMANDS, and that
/// was a live defect. `extract_cmds` returned everything after `"cmd":` verbatim, so
/// the JSON delimiter stayed on the front. The guard rejects a match preceded by a
/// quote -- meaning SHELL quoting -- and cannot tell the two apart, so
///
///     "cmd": "python3 scripts/check-x.py --gate"
///
/// was rejected as quoted and read as an ORPHAN. Measured on main before this change:
/// that shape returned false while the `./` shape returned true, and it returned true
/// only because the `./` clause consulted no guard at all. The unguarded clause was
/// LOAD-BEARING -- guarding it without decoding first turns every DAG-scheduled
/// checker into a false orphan at once.
fn decode_json_string(raw: &str) -> String {
    let Some(rest) = raw.strip_prefix('"') else {
        return raw.to_string();
    };
    let mut out = String::with_capacity(rest.len());
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => break,
            },
            other => out.push(other),
        }
    }
    out
}


/// Pull the value of every `"cmd":` field out of a DAG file.
///
/// Deliberately textual rather than a JSON parse: these scripts take no third-party
/// dependencies, and the field is emitted one-per-line by the generator. It fails
/// closed at the call site if the result is empty, so a schema move is loud rather
/// than silently reporting every checker as an orphan.
fn extract_cmds(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let Some(rest) = line.split_once("\"cmd\"") else {
            continue;
        };
        let Some(after_colon) = rest.1.split_once(':') else {
            continue;
        };
        out.push(decode_json_string(after_colon.1.trim()));
    }
    out
}

/// Extract just the `lint-checks` recipe. Using the whole Makefile would count a
/// checker named anywhere in it -- including in `lint-cargo`, in a comment, or in
/// an unrelated target -- as scheduled, which is the false-negative this guard
/// exists to prevent.
fn lint_checks_recipe(makefile: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in makefile.lines() {
        if line.starts_with("lint-checks:") {
            inside = true;
            continue;
        }
        if inside {
            // A recipe line is TAB-indented; anything else ends the recipe.
            if line.starts_with('\t') {
                let t = line.trim_start();
                if !t.starts_with('#') {
                    out.push_str(line);
                    out.push('\n');
                }
            } else if !line.trim().is_empty() {
                break;
            }
        }
    }
    assert!(
        !out.is_empty(),
        "could not find a `lint-checks:` recipe in the Makefile; this guard would \
         otherwise report every lint checker as an orphan"
    );
    out
}

fn self_test() {
    // Comment stripping is what separates a real invocation from a mention.
    let sh = "# ./scripts/check-a.sh mentioned in a comment\n./scripts/check-b.sh\n";
    let stripped = strip_comments(sh, "x.sh");
    assert!(!stripped.contains("check-a.sh"), "commented-out shell line survived");
    assert!(stripped.contains("check-b.sh"), "live shell line was dropped");

    let rs = "// see scripts/check-c.rs for the equivalence proof\nrun(\"scripts/check-d.rs\");\n";
    let stripped = strip_comments(rs, "x.rs");
    assert!(!stripped.contains("check-c.rs"), "commented-out rust line survived");
    assert!(stripped.contains("check-d.rs"), "live rust line was dropped");

    // The real case that made this necessary.
    let real = "# Equivalence is TESTED, not asserted -- see check-detcore-backend-abstraction-test.sh:\n";
    assert!(
        strip_comments(real, "check-detcore-backend-abstraction.sh").is_empty(),
        "the comment-only reference that fooled the manual sweep still reads as an invocation"
    );

    // The recipe extractor must take lint-checks and stop before lint-cargo.
    let mk = "lint: lint-checks lint-cargo\n\nlint-checks:\n\t./scripts/check-x.sh\n\t@git diff --check\n\nlint-cargo:\n\t$(CARGO) clippy\n";
    let recipe = lint_checks_recipe(mk);
    assert!(recipe.contains("check-x.sh"), "recipe line missing");
    assert!(!recipe.contains("clippy"), "lint-cargo leaked into the lint-checks recipe");

    // This checker's own ALLOWLIST must not count as scheduling evidence.
    assert!(!expands_frontier(SELF_PATH), "this file's body is being read as invocations");
    assert!(expands_frontier("ci/run-reverie-pin-check.sh"), "a real intermediary stopped expanding");
    assert!(
        ALLOWLIST.iter().all(|(p, _)| *p != SELF_PATH),
        "this checker allowlisting itself would hide the very orphan it reports"
    );

    // ⚠️ A FLAG BETWEEN THE RUNNER AND THE PATH. The real cmd from check.exit_status_class
    // (hermit#2655), which this guard called an orphan while it was genuinely scheduled.
    let flagged = "mkdir -p target/ci && RUSTUP_TOOLCHAIN=stable rustc --edition=2021 \
scripts/check-exit-status-class.rs -o target/ci/check-exit-status-class && \
target/ci/check-exit-status-class --gate";
    assert!(
        is_invoked(flagged, "scripts/check-exit-status-class.rs"),
        "a flag between the runner and the path hid a scheduled checker"
    );
    assert!(
        is_invoked("rustc scripts/check-x.rs", "scripts/check-x.rs"),
        "the adjacent form regressed"
    );
    assert!(
        is_invoked("python3 -B scripts/check-y.py", "scripts/check-y.py"),
        "a flag before a python path hid a scheduled checker"
    );
    // ⚠️ THE KNOWN GAP, PINNED RATHER THAN LEFT TO BE REDISCOVERED. A flag taking a
    // SEPARATE value (`-X faulthandler`, `-o out`) stops the scan at the value, so this
    // form still reads as an orphan. Closing it needs a table of which flags consume the
    // next token; a wrong entry there fails SILENTLY, by reading an output as an input.
    // The false orphan is loud and costs one line to fix, so it is the side to be on.
    // If you widen this, keep the `-o` control below passing.
    assert!(
        is_invoked("python3 -X faulthandler scripts/check-y.py", "scripts/check-y.py"),
        "an unrecognised separate-value flag must no longer hide the script after it"
    );
    // The COST of the new default, pinned so it is a choice and not a surprise: a
    // boolean missing from NO_VALUE_FLAGS is assumed to take a value, so the scan
    // steps past the script and reports a FALSE ORPHAN. Loud, and that is the point
    // -- the remedy is to add the flag, not to widen the default back.
    assert!(
        !is_invoked("python3 --nonsense-bool scripts/check-y.py", "scripts/check-y.py"),
        "an unknown boolean is assumed to take a value; add it to NO_VALUE_FLAGS"
    );
    // ⚠️ THE ORDERING THE ORIGINAL CONTROL MISSED, AND IT WAS A LIVE FALSE
    // "SCHEDULED". `-o` directly after the runner was skipped as an ordinary flag,
    // so the scan landed on its VALUE and read an OUTPUT as an invocation. The
    // control below uses the input-first ordering, where the scan stops at
    // `other.rs` and never reaches `-o`, so it passed while this did not.
    assert!(
        !is_invoked("rustc -o scripts/check-z.rs other.rs", "scripts/check-z.rs"),
        "an output path directly after -o read as an invocation -- a false SCHEDULED, \
         which is the silent direction this guard must never fail in"
    );
    // A value-taking flag must not hide the script that follows its value. Without
    // the table this stops on `2021` and reports a false ORPHAN.
    assert!(
        is_invoked("rustc --edition 2021 scripts/check-x.rs", "scripts/check-x.rs"),
        "a separate-value flag hid the script after it"
    );
    assert!(
        is_invoked("rustc -C opt-level=3 scripts/check-x.rs", "scripts/check-x.rs"),
        "a -C value hid the script after it"
    );
    // ⚠️ THE SHARED CONTROL. Four matcher defects in one night shared a SIGNATURE and
    // not a cause: each inferred a semantic fact from an unverified syntactic position,
    // and each failed DOWNWARD, where a smaller count reads as fewer problems. A shared
    // signature buys no shared patch -- the fixes below are independent -- but it does
    // buy this: a block of cases that must be FALSE, so a narrowing cannot pass as green.
    // Add to it whenever a clause is hardened; that is what none of the three earlier
    // fixes did, which is why each was defeated by the next input shape.
    for (text, path, why) in [
        (
            "rustc -o ./scripts/check-z.rs in.rs",
            "scripts/check-z.rs",
            "a dotted -o OUTPUT read as an invocation -- this bypassed the flag scan entirely",
        ),
        (
            "python3 -c ./scripts/check-z.py x.py",
            "scripts/check-z.py",
            "a dotted -c VALUE read as an invocation",
        ),
        (
            "sh -c \"echo ./scripts/check-z.rs\"",
            "scripts/check-z.rs",
            "a path inside a shell-quoted string read as an invocation",
        ),
        (
            "OUT=./scripts/check-z.rs",
            "scripts/check-z.rs",
            "an assignment VALUE read as an invocation",
        ),
        (
            "see ./scripts/check-z.rs for details",
            "scripts/check-z.rs",
            "a mid-sentence mention read as an invocation",
        ),
    ] {
        assert!(!is_invoked(text, path), "false SCHEDULED (silent direction): {why}");
    }

    // Pins the OPENING-delimiter rule specifically: a quote immediately before the
    // path is only a delimiter when what precedes IT can open one. Here it cannot,
    // so this is an attached flag value and not a command.
    assert!(
        !is_invoked("rustc -o\"./scripts/check-z.rs\" in.rs", "scripts/check-z.rs"),
        "an attached -o\"value\" read as an invocation"
    );
    // Pins JSON DECODING, which nothing else reaches: every other case here calls
    // is_invoked on text that is already decoded, so dropping the decoder leaves them
    // all green while every DAG command silently changes shape.
    assert_eq!(
        extract_cmds("      \"cmd\": \"python3 scripts/check-x.py --gate\","),
        vec!["python3 scripts/check-x.py --gate".to_string()],
        "a DAG command must reach the matcher as shell text, not as a JSON literal"
    );

    // ...and the other half of the trade. Every one of these is a REAL invocation in
    // this repository today; if the guard above is tightened until one of them fails,
    // the checker starts reporting scheduled checkers as orphans.
    for (text, path, why) in [
        (
            "\t$(SUBMODULE_PROXY) ./scripts/check-n.rs",
            "scripts/check-n.rs",
            "a make variable expansion before the command",
        ),
        (
            "FOO=bar ./ci/node.sh",
            "ci/node.sh",
            "an environment assignment before the command",
        ),
        (
            "python3 scripts/check-x.py --gate",
            "scripts/check-x.py",
            "a DAG command in runner form, after JSON decoding",
        ),
        (
            "n.cmd = \"./ci/verify-x.sh p\".to_string();",
            "ci/verify-x.sh",
            "a command built as a Rust string literal in scripts/validate.rs",
        ),
        (
            "rustc --edition 2021 scripts/check-x.rs",
            "scripts/check-x.rs",
            "a separate-value flag between the runner and the script",
        ),
        (
            "cargo build && scripts/check-y.sh --gate",
            "scripts/check-y.sh",
            "a checker CHAINED after another command -- the bare-path clause used to \
             require the START OF A LINE, so every `a && checker` shape in a Makefile \
             or DAG command read as an orphan",
        ),
    ] {
        assert!(is_invoked(text, path), "false ORPHAN (loud direction): {why}");
    }

    // ⚠️ THE CONTROL FOR THE SILENT DIRECTION. An OUTPUT path must not read as an
    // invocation: matching it would report an unscheduled checker as scheduled, which is
    // the failure this whole guard exists to prevent and is invisible when wrong.
    assert!(
        !is_invoked("rustc other.rs -o scripts/check-z.rs", "scripts/check-z.rs"),
        "an -o output path was read as an invocation"
    );
    assert!(
        !is_invoked("see scripts/check-w.sh for details", "scripts/check-w.sh"),
        "a mid-sentence mention was read as an invocation"
    );

    println!("PASS: check-checker-scheduling strips comments, keeps invocations, scopes the recipe, and does not read its own allowlist as scheduling");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commented_reference_is_not_an_invocation() {
        let s = strip_comments("# ./scripts/check-a.sh\n./scripts/check-b.sh\n", "x.sh");
        assert!(!s.contains("check-a.sh"));
        assert!(s.contains("check-b.sh"));
    }

    #[test]
    fn rust_line_comments_are_stripped_too() {
        let s = strip_comments("// scripts/check-c.rs\nrun(\"check-d.rs\");\n", "x.rs");
        assert!(!s.contains("check-c.rs"));
        assert!(s.contains("check-d.rs"));
    }

    /// ⚠️ THE FIXTURE CASE, WHICH IS THE ONE THAT REDDENED MAIN. `ci/lint-checks-node.sh`
    /// hands `check_run` a quoted multi-line string standing in for make's output, and
    /// its first line is `"python3 scripts/test_validate_stop_paths.py`. Requiring the
    /// `python3 ` prefix was not enough -- the fixture has it. Measured 2026-08-26:
    /// this exact line made a correctly-allowlisted checker report STALE on main.
    #[test]
    fn a_runner_prefix_inside_a_string_literal_is_not_an_invocation() {
        let fixture = "        \"python3 scripts/test_validate_stop_paths.py";
        assert!(
            !is_invoked(fixture, "scripts/test_validate_stop_paths.py"),
            "a runner prefix quoted as fixture data must not read as an invocation"
        );
    }

    /// ...and the real shapes must still fire, so the guard cannot be over-tightened
    /// into a permanent pass. Each of these is a form this repository actually uses.
    #[test]
    fn genuine_invocations_still_fire_after_the_quote_guard() {
        for real in [
            "\tpython3 scripts/test_validate_stop_paths.py",
            "python3 scripts/test_validate_stop_paths.py",
            "\t./scripts/check-x.sh",
            "\tcd foo && python3 scripts/test_validate_stop_paths.py",
            "\t$(SUBMODULE_PROXY) ./ci/verify-submodules.sh",
        ] {
            let path = if real.contains("check-x") {
                "scripts/check-x.sh"
            } else if real.contains("verify-submodules") {
                "ci/verify-submodules.sh"
            } else {
                "scripts/test_validate_stop_paths.py"
            };
            assert!(is_invoked(real, path), "must still fire: {real}");
        }
    }

    #[test]
    fn recipe_extraction_stops_at_the_next_target() {
        let mk = "lint-checks:\n\t./scripts/check-x.sh\n\nlint-cargo:\n\t$(CARGO) clippy\n";
        let r = lint_checks_recipe(mk);
        assert!(r.contains("check-x.sh"));
        assert!(!r.contains("clippy"));
    }

    #[test]
    fn a_makefile_without_the_recipe_is_an_error_not_an_empty_pass() {
        let r = std::panic::catch_unwind(|| lint_checks_recipe("lint:\n\techo hi\n"));
        assert!(r.is_err(), "a missing lint-checks recipe must refuse, not return empty");
    }

    /// The regression that scheduling this checker introduced: its own ALLOWLIST
    /// literals were read as invocations, so all five allowlisted checkers
    /// reported STALE and `make lint-checks` exited 1.
    #[test]
    fn this_checkers_own_body_is_not_an_invocation_source() {
        assert!(!expands_frontier(SELF_PATH));
        assert!(expands_frontier("scripts/validate.rs"));
        assert!(expands_frontier("ci/run-reverie-pin-check.sh"));
    }

    /// Excluding the body must not become excluding the file: it is a checker
    /// like any other and still has to be scheduled by something.
    #[test]
    fn this_checker_does_not_allowlist_itself() {
        assert!(ALLOWLIST.iter().all(|(p, _)| *p != SELF_PATH));
    }

    #[test]
    fn every_allowlist_entry_carries_a_nonempty_reason() {
        for (path, reason) in ALLOWLIST {
            assert!(!path.is_empty());
            assert!(
                reason.len() > 40,
                "allowlist entry {path} needs a real reason, not a placeholder"
            );
        }
    }

    // The regression that motivated is_invoked: a filename quoted inside a live
    // (non-comment) diagnostic message is NOT an invocation. ci/lint-checks-node.sh
    // echoes exactly this shape, and a substring match read it as scheduling.
    #[test]
    fn a_name_quoted_mid_sentence_is_not_an_invocation() {
        let echoed = "        echo '  contract in scripts/test_validate_stop_paths.py exercises the REAL parent' >&2";
        assert!(!is_invoked(echoed, "scripts/test_validate_stop_paths.py"));
    }

    #[test]
    fn the_shapes_that_do_run_a_script_all_count() {
        assert!(is_invoked("\t./scripts/check-x.sh", "scripts/check-x.sh"));
        assert!(is_invoked(
            "\tpython3 scripts/test_x.py",
            "scripts/test_x.py"
        ));
        assert!(is_invoked(
            "\t$(SUBMODULE_PROXY) ./ci/run-x.sh",
            "ci/run-x.sh"
        ));
        // Compiled rather than interpreted, and split across a continuation line --
        // how run-reverie-pin-check.sh reaches both pin checkers.
        assert!(is_invoked(
            "    RUSTUP_TOOLCHAIN=stable rustc --edition=2021 \\\n        scripts/check-reverie-pin.rs -o \"$checker\"",
            "scripts/check-reverie-pin.rs"
        ));
    }

    // A DAG `description` is prose and routinely names checkers it does not run.
    #[test]
    fn only_cmd_fields_seed_scheduling_not_descriptions() {
        let dag = r#"{"steps":[
          {"job":"a","description":"this node supersedes scripts/check-ghost.sh entirely",
           "cmd": "./scripts/check-real.sh"}]}"#;
        let cmds = extract_cmds(dag);
        assert_eq!(cmds.len(), 1, "one cmd expected, got {cmds:?}");
        assert!(cmds[0].contains("check-real.sh"));
        assert!(
            !cmds[0].contains("check-ghost.sh"),
            "a checker named only in prose must not read as scheduled"
        );
    }
}
