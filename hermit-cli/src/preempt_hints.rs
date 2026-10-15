/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Source-level preemption hints, `hermit run --preempt-at`
//! (<https://github.com/rrnewton/hermit/issues/4371>): the spec, and its strict
//! resolution against the guest program's symbol table and DWARF line
//! program.
//!
//! A hint names a code location in the main executable. Each time a guest
//! thread reaches an instruction there and the hint's selector picks that
//! visit, the thread's time slice ends as a timer preemption would end it.
//! This module only parses and resolves. Resolution is strict: a location
//! that does not name exactly the code asked for is refused, never moved
//! somewhere nearby.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use crate::happens_before::DebugInfoResolver;

/// Where a hint is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HintLocation {
    /// A function's entry, by its raw symbol name.
    Function(String),
    /// A source line, scoped by a source file's basename or, when the scope
    /// names no source file, by a function.
    Line {
        /// The file or function the line belongs to.
        scope: String,
        /// The 1-based source line.
        line: u32,
    },
}

/// Which thread's visits a hint counts and acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintThread {
    /// The program's first thread.
    Main,
    /// The Nth thread the program spawned (1-based), as the happens-before
    /// `spawn_ordinal` counts.
    Spawn(u32),
    /// The thread with this deterministic thread id.
    DetTid(u32),
}

/// Which visits of a thread end its time slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintSelect {
    /// Every Kth visit (K >= 1).
    Every(u64),
    /// Only the Nth visit (N >= 1).
    Nth(u64),
}

/// One `--preempt-at` argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HintSpec {
    /// The argument as given, for messages.
    pub text: String,
    /// Where the hint is.
    pub location: HintLocation,
    /// Whose visits count; every thread's when `None`.
    pub thread: Option<HintThread>,
    /// Which visits end the slice.
    pub select: HintSelect,
}

impl fmt::Display for HintSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// A positive count, for `every=` and `nth=`.
fn positive(key: &str, value: &str) -> Result<u64, String> {
    match value.parse::<u64>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!(
            "{key}= needs a whole number of at least 1, not {value:?}"
        )),
    }
}

fn parse_thread(value: &str) -> Result<HintThread, String> {
    let ordinal = |s: &str| s.parse::<u32>().ok().filter(|n| *n >= 1);
    if value == "main" {
        Ok(HintThread::Main)
    } else if let Some(n) = value.strip_prefix("spawn:").and_then(ordinal) {
        Ok(HintThread::Spawn(n))
    } else if let Some(n) = value.strip_prefix("dettid:").and_then(|s| s.parse().ok()) {
        Ok(HintThread::DetTid(n))
    } else {
        Err(format!(
            "thread= is main, spawn:N (N >= 1) or dettid:N, not {value:?}"
        ))
    }
}

impl FromStr for HintSpec {
    type Err = String;

    /// Parses `LOCATION[,thread=T][,every=K | ,nth=N]`, where LOCATION is
    /// `func`, `file:line` or `func:line`.
    fn from_str(text: &str) -> Result<Self, String> {
        let mut parts = text.split(',');
        let location_text = parts.next().unwrap_or_default();
        if location_text.is_empty() {
            return Err("a location is needed: func, file:line or func:line".to_owned());
        }
        // A trailing `:N` is a line; a name may itself contain colons
        // (C++'s `ns::f`), so only the last one counts.
        let location = match location_text.rsplit_once(':') {
            Some((scope, line)) if !scope.is_empty() && line.parse::<u32>().is_ok() => {
                let line: u32 = line.parse().expect("checked above");
                if line == 0 {
                    return Err(format!("{location_text:?}: lines are numbered from 1"));
                }
                HintLocation::Line {
                    scope: scope.to_owned(),
                    line,
                }
            }
            _ => HintLocation::Function(location_text.to_owned()),
        };
        let mut thread = None;
        let mut select = None;
        for part in parts {
            let (key, value) = part.split_once('=').ok_or_else(|| {
                format!("{part:?} is not key=value; the keys are thread, every and nth")
            })?;
            match key {
                "thread" if thread.is_none() => thread = Some(parse_thread(value)?),
                "every" | "nth" if select.is_none() => {
                    let n = positive(key, value)?;
                    select = Some(if key == "every" {
                        HintSelect::Every(n)
                    } else {
                        HintSelect::Nth(n)
                    });
                }
                "thread" | "every" | "nth" => {
                    return Err(format!(
                        "{key}= given twice, or every= with nth=; give at most one thread= and \
                         one of every= and nth="
                    ));
                }
                _ => {
                    return Err(format!(
                        "unknown key {key:?}; the keys are thread, every and nth"
                    ));
                }
            }
        }
        Ok(HintSpec {
            text: text.to_owned(),
            location,
            thread,
            select: select.unwrap_or(HintSelect::Every(1)),
        })
    }
}

/// A hint with every address it covers, in ascending order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedHint {
    /// The hint as given.
    pub spec: HintSpec,
    /// Its link-time addresses in the program: a function's entry, or every
    /// statement address of a line.
    pub addrs: Vec<u64>,
}

/// Resolves every hint against `resolver`, built from the program `program`
/// (named in messages). Strict:
/// - a function hint needs a symbol of exactly that name;
/// - a line hint takes every `is_stmt` address of the line, in the file it
///   names, or within the function it names; a line not found there is
///   refused, never moved to the function's entry;
/// - two hints may not share an address, so one stop belongs to at most one
///   hint.
///
/// A scope that names a source file in the line table is a file; otherwise it
/// must name a function. Every problem is reported, not only the first.
pub fn resolve_hints(
    specs: &[HintSpec],
    resolver: &DebugInfoResolver,
    program: &str,
) -> Result<Vec<ResolvedHint>, String> {
    let mut problems = Vec::new();
    let mut resolved = Vec::new();
    for spec in specs {
        let addrs = match &spec.location {
            HintLocation::Function(name) => match resolver.function_addr(name) {
                Some(addr) => vec![addr],
                None => {
                    problems.push(format!(
                        "--preempt-at {spec}: {program} has no function symbol {name:?}. \
                         Names are matched as raw symbols, so for Rust or C++ code write \
                         file:line instead"
                    ));
                    continue;
                }
            },
            HintLocation::Line { scope, line } => {
                if resolver.has_line_file(scope) {
                    resolver.stmt_line_addrs(Some(scope), *line, None)
                } else if let Some((addr, size)) = resolver.function_extent(scope) {
                    if size == 0 {
                        problems.push(format!(
                            "--preempt-at {spec}: function {scope:?} has no size in {program}'s \
                             symbol table, so its lines cannot be bounded; write file:line"
                        ));
                        continue;
                    }
                    resolver.stmt_line_addrs(None, *line, Some((addr, size)))
                } else {
                    problems.push(format!(
                        "--preempt-at {spec}: {scope:?} names neither a source file in \
                         {program}'s line table nor a function in its symbol table; check the \
                         spelling (a file is matched by its basename), and build the program \
                         with -g"
                    ));
                    continue;
                }
            }
        };
        if addrs.is_empty() {
            problems.push(format!(
                "--preempt-at {spec}: {program}'s line table has no statement address for that \
                 line there; name a line that has code, and check the addresses with \
                 --preempt-at-list"
            ));
            continue;
        }
        resolved.push(ResolvedHint {
            spec: spec.clone(),
            addrs,
        });
    }
    let mut owner: BTreeMap<u64, &HintSpec> = BTreeMap::new();
    for hint in &resolved {
        for addr in &hint.addrs {
            if let Some(other) = owner.insert(*addr, &hint.spec) {
                problems.push(format!(
                    "--preempt-at {other} and --preempt-at {} share the address {addr:#x}; \
                     hints may not overlap, so that one stop belongs to one hint",
                    hint.spec
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(resolved)
    } else {
        Err(problems.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn spec(text: &str) -> HintSpec {
        text.parse()
            .unwrap_or_else(|e| panic!("{text:?} should parse: {e}"))
    }

    #[test]
    fn parses_locations_threads_and_selectors() {
        assert_eq!(
            spec("square").location,
            HintLocation::Function("square".into())
        );
        assert_eq!(
            spec("hints.c:12").location,
            HintLocation::Line {
                scope: "hints.c".into(),
                line: 12
            }
        );
        assert_eq!(
            spec("ns::f").location,
            HintLocation::Function("ns::f".into())
        );
        assert_eq!(
            spec("ns::f:3").location,
            HintLocation::Line {
                scope: "ns::f".into(),
                line: 3
            }
        );
        let full = spec("square:2,thread=spawn:1,nth=3");
        assert_eq!(full.thread, Some(HintThread::Spawn(1)));
        assert_eq!(full.select, HintSelect::Nth(3));
        assert_eq!(spec("square").select, HintSelect::Every(1));
        assert_eq!(spec("square,every=4").select, HintSelect::Every(4));
        assert_eq!(spec("square,thread=main").thread, Some(HintThread::Main));
        assert_eq!(
            spec("square,thread=dettid:7").thread,
            Some(HintThread::DetTid(7))
        );
        assert_eq!(spec("square,nth=2").to_string(), "square,nth=2");
    }

    #[test]
    fn refuses_malformed_specs() {
        for (text, needle) in [
            ("", "a location is needed"),
            ("f.c:0", "numbered from 1"),
            ("f,every=0", "at least 1"),
            ("f,nth=x", "at least 1"),
            ("f,every=2,nth=3", "one of every= and nth="),
            ("f,thread=main,thread=main", "given twice"),
            ("f,thread=spawn:0", "thread= is main"),
            ("f,colour=red", "unknown key"),
            ("f,nth", "not key=value"),
        ] {
            let error = text.parse::<HintSpec>().expect_err(text);
            assert!(error.contains(needle), "{text:?}: {error}");
        }
    }

    /// A -g fixture with three functions on separate lines, in its own
    /// directory, removed when the returned guard drops. A missing compiler
    /// fails the test rather than skipping it.
    fn compile_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("failed to create the fixture directory");
        let src = dir.path().join("hints.c");
        let bin = dir.path().join("hints");
        std::fs::write(
            &src,
            "int square(int x) {\n\
             \x20   int y = x * x;\n\
             \x20   return y;\n\
             }\n\
             int twice(int x) {\n\
             \x20   return x + x;\n\
             }\n\
             int main(void) {\n\
             \x20   return square(3) + twice(2);\n\
             }\n",
        )
        .expect("failed to write the fixture source");
        let output = Command::new("cc")
            .args(["-g", "-O0", "-o"])
            .arg(&bin)
            .arg(&src)
            .output()
            .expect("failed to run cc for the fixture");
        assert!(
            output.status.success(),
            "fixture compilation failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        (dir, bin)
    }

    #[test]
    fn resolves_strictly_and_refuses_overlap() {
        let (_dir, bin) = compile_fixture();
        let resolver = DebugInfoResolver::open(&bin).unwrap();
        let (square, square_size) = resolver.function_extent("square").unwrap();

        // A function's entry; a file line; a line within a function.
        let hints = resolve_hints(
            &[spec("twice"), spec("hints.c:2"), spec("main:9")],
            &resolver,
            "hints",
        )
        .unwrap();
        assert_eq!(
            hints[0].addrs,
            vec![resolver.function_addr("twice").unwrap()]
        );
        assert!(!hints[1].addrs.is_empty());
        assert!(
            hints[1]
                .addrs
                .iter()
                .all(|a| (square..square + square_size).contains(a)),
            "line 2 is inside square: {:x?}",
            hints[1].addrs
        );
        assert!(!hints[2].addrs.is_empty());

        // No fallback: square has no line 9, so square:9 is refused rather
        // than moved to square's entry.
        let error = resolve_hints(&[spec("square:9")], &resolver, "hints").unwrap_err();
        assert!(error.contains("no statement address"), "{error}");

        // Unknown names.
        let error = resolve_hints(&[spec("nosuch")], &resolver, "hints").unwrap_err();
        assert!(error.contains("no function symbol"), "{error}");
        let error = resolve_hints(&[spec("nosuch:3")], &resolver, "hints").unwrap_err();
        assert!(error.contains("names neither"), "{error}");

        // Overlap: square's entry is also the first statement of its line 1.
        let error =
            resolve_hints(&[spec("square"), spec("hints.c:1")], &resolver, "hints").unwrap_err();
        assert!(
            error.contains("share the address") && error.contains("may not overlap"),
            "{error}"
        );

        let _ = std::fs::remove_dir_all(bin.parent().unwrap());
    }
}
