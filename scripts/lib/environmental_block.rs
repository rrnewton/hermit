// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Shared classification of captured output that shows an environmental block.

/// Phrases that mean "the kernel/sandbox said no", in lowercase.
const DENIALS: &[&str] = &[
    "operation not permitted",
    "permission denied",
    "(os error 1)",
];

fn has_denial(line: &str) -> bool {
    DENIALS.iter().any(|d| line.contains(d))
}

fn has_any(line: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| line.contains(n))
}

/// Build-tool / compiler / linker phrasing for a Form-2 denial, in lowercase.
///
/// Split out of the old inline test so the same anchors can be applied per BLOCK
/// (see [`diagnostic_blocks`]) instead of per physical line.
fn has_toolchain_phrase(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("error: failed to write ")
        || trimmed.starts_with("rm: cannot remove ")
        || (line.contains("fatal error: ") && line.matches(':').count() >= 2)
        || line.contains("cmake error")
        || has_any(
            line,
            &[
                "cannot open",
                "error opening",
                "failed to open",
                "could not open",
                "opening dependency file",
                "could not write output to",
                "couldn't create the temp file",
                "can't create",
                "cannot execute",
                "could not create temporary file",
                "failed to build archive at",
                // Measured 2026-08-17: rustc's incremental link_or_copy step
                // says "unable to copy" where the sibling path says "could
                // not write output to".
                "unable to copy",
                // A same-line cargo spawn denial retains the historical
                // classification. Cross-line matching is narrower below.
                "could not execute process",
                // `cp -a`/`ln -s` uses this spelling rather than "can't
                // create".
                "cannot create symbolic link",
            ],
        )
}

/// Tool syntax proven safe to join with a denial elsewhere in the same Cargo
/// diagnostic block. Generic phrases such as "could not open" stay same-line
/// only: product prose can contain them above an indented guest EPERM.
fn has_cross_line_toolchain_phrase(line: &str) -> bool {
    line.trim_start().starts_with("could not execute process ")
}

/// Group `lower` into diagnostic blocks: one leading line plus its continuations.
///
/// Cargo can put the tool phrase and the denial on separate lines:
///
/// ```text
/// error: failed to run custom build command for `libm v0.2.16`
///
/// Caused by:
///   could not execute process .../build-script-build (never executed)
///
/// Caused by:
///   Permission denied (os error 13)
/// ```
///
/// A block ends at the next unindented line. Continuations are blank lines,
/// indented lines, and the bare `Caused by:` header, matching Cargo's own error
/// grouping without widening the search across unrelated output. A jail banner
/// is unindented, so it cannot be absorbed into a genuine compiler-error block.
fn diagnostic_blocks(lower: &str) -> Vec<Vec<&str>> {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in lower.lines() {
        let trimmed = line.trim_start();
        let continues = trimmed.is_empty()
            || line.starts_with(' ')
            || line.starts_with('\t')
            || trimmed.starts_with("caused by:");
        if continues && !blocks.is_empty() {
            blocks.last_mut().expect("checked non-empty").push(line);
        } else {
            blocks.push(vec![line]);
        }
    }
    blocks
}

/// Classify a captured output region as an environmental block.
///
/// Misclassifying a product failure as environmental is as harmful as the
/// reverse, so banner-less anchors stay pinned to build-tool or VCS phrasing.
/// Ordinary guest output that contains EPERM must not trip them. The classes are:
///
/// - `bpfjailer-banner`: the canonical jail banner or its own telemetry pointer.
/// - `toolchain-eperm`: a compiler, linker, CMake, or Cargo denial.
/// - `third-party-build`: the measured vendored Reverie DBI build failure.
/// - `proxy-egress`: the forward proxy could not reach the required host.
/// - `vcs-fs-denial`: a banner-less Git filesystem denial.
///
/// What this returns is distinct from the retry verdict: a run can observe a
/// denial here and later refute that classification by failing again on a real
/// product cause. The two must not be collapsed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EnvBlockObservation {
    /// A denial fired, and this names its form.
    Denied(&'static str),
    /// Evidence was present and carried no denial. A product failure here is real.
    NoDenial,
    /// There was nothing to evaluate. This is not evidence that no denial occurred.
    #[default]
    NothingObserved,
}

impl EnvBlockObservation {
    /// The legacy two-state view, for callers that ask only whether a denial was
    /// observed. `NothingObserved` maps to `None`, which is why callers that need
    /// to distinguish it from `NoDenial` use the three-state form directly.
    pub fn class(self) -> Option<&'static str> {
        match self {
            Self::Denied(class) => Some(class),
            Self::NoDenial | Self::NothingObserved => None,
        }
    }
}

/// Preserved two-state entry point for callers that ask only whether a denial
/// was observed.
pub fn environmental_block_class(output: &str) -> Option<&'static str> {
    environmental_block_observation(output).class()
}

pub fn environmental_block_observation(output: &str) -> EnvBlockObservation {
    // Nothing to evaluate is its own answer. It does not mean that a nonempty
    // captured region was examined and found to contain no denial.
    if output.trim().is_empty() {
        return EnvBlockObservation::NothingObserved;
    }
    let lower = output.to_ascii_lowercase();

    // The unanchored bpfjailer match is deliberate. It accepts some mention-only
    // text, including "no bpfjailer denial was observed this run", because the
    // jailer's own pointer line contains `scuba bpfjailer_enforce` without a
    // denial word. Narrowing this requires a replacement that preserves that
    // measured positive case and therefore an owner decision about which error
    // to prefer.
    if lower.contains("blocked on this server based on a security policy")
        || lower.contains("enforcer: fs, reason:")
        || lower.contains("enforcer: exec, reason:")
        || lower.contains("enforcer: net, reason:")
        || lower.contains("bpfjailer")
    {
        return EnvBlockObservation::Denied("bpfjailer-banner");
    }
    if (lower.contains("failed to run custom build command for") && lower.contains("reverie-dbi"))
        || (lower.contains("panicked at") && lower.contains("reverie-dbi/build.rs"))
    {
        return EnvBlockObservation::Denied("third-party-build");
    }
    let mut vcs_hit = false;
    for line in lower.lines() {
        // Egress failures are host/network evidence, not a Hermit result.
        if has_any(
            line,
            &[
                "could not resolve proxy",
                "could not resolve host",
                "connect tunnel failed, response 403",
                "proxy connect aborted",
                "failed to connect to fwdproxy",
            ],
        ) {
            return EnvBlockObservation::Denied("proxy-egress");
        }
        if has_denial(line) && has_toolchain_phrase(line) {
            return EnvBlockObservation::Denied("toolchain-eperm");
        }
        if (line.starts_with("fatal:") || line.contains(" fatal: ") || line.contains(".git/"))
            && has_any(
                line,
                &[
                    "cannot mkdir",
                    "could not create work tree dir",
                    "could not create leading directories",
                    "unable to create",
                    "unable to write",
                    "unable to access",
                    "could not lock",
                    "chmod on",
                    ".git/",
                ],
            )
        {
            vcs_hit = true;
        }
    }
    // Only Cargo's structural phrase may join a denial elsewhere in the same
    // diagnostic block. Generic phrases remain same-line above.
    if diagnostic_blocks(&lower).iter().any(|block| {
        block.iter().any(|line| has_denial(line))
            && block
                .iter()
                .any(|line| has_cross_line_toolchain_phrase(line))
    }) {
        return EnvBlockObservation::Denied("toolchain-eperm");
    }
    if vcs_hit {
        return EnvBlockObservation::Denied("vcs-fs-denial");
    }
    EnvBlockObservation::NoDenial
}
