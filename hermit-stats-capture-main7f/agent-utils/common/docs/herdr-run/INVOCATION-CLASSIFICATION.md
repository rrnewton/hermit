# What the allowlist can and cannot cover

The classification first. Mechanism proposals are at the end, deliberately, because the list is the
part that stops us rediscovering this one component at a time.

## The structural limit, stated once

herdr-run admits a command by matching **the program name it is handed**. Whatever that program then
spawns inherits the pane's egress without any further check — verified, not assumed: routing
`git commit` through the relay let a consuming repository's own tracked `pre-commit` hook run its
`git ls-remote` against github, which is exactly the behaviour the in-jail path denies.

Two consequences follow, and they are the whole problem:

1. **A wrapper cannot be usefully allowlisted.** Allowlist `make`, `rust-script`, `bash` or
   `python3` and you have allowlisted every program they can invoke — which is all of them. The
   allowlist becomes decorative. Refuse them and the relay simply cannot serve that invocation.
   There is no middle setting.
2. **"Direct" is not a property of the program name.** It is a property of whether the program can
   spawn arbitrary code. A program that runs user-supplied hooks, build scripts or plugins is a
   wrapper wearing a fixed name.

## The classification

### (a) DIRECT — eligible for a constrained positive policy

| Program | Why it qualifies | Status and residual |
| --- | --- | --- |
| `gh` | Fixed subcommand surface; the code-executing subcommands are enumerable | Enabled by default; `alias`, `extension`/`ext`, and `codespace` are denied |
| `cargo` | A positive list can restrict the visible subcommand surface to dependency-oriented operations | **Disabled by default.** A project may opt in to the constrained subcommands, but even `fetch` can execute helpers selected by ambient Cargo configuration. `build`/`test`/`run`/`install`/`clippy` and every third-party `cargo-*` remain refused; a deny-list would be the wrong shape because the code-executing set is open-ended |

### (a′) DIRECT IN NAME, WRAPPER IN FACT — the leak in the category we thought was safe

| Program | How it escapes | Status |
| --- | --- | --- |
| `git` | Repo-controlled hooks. A repository that sets `core.hooksPath` to a tracked directory makes `git commit`, `git push`, `git merge` execute repository-supplied scripts **with pane egress**. Measured against a real consumer. | **OPEN.** `git` is the first thing we allowlisted and it is the widest hole in the door. |

This is not hypothetical and it is not only a security point: it is also *why* routing `git commit`
through the relay fixed a consumer's pin-check hook. The same mechanism that makes it useful makes it
unbounded. Any tightening must not break that use.

### (b) WRAPPED — cannot be fixed by adding an allowlist entry

Counted in this tree, by entry-point shape rather than by file:

| Shape | Count | Examples |
| --- | --- | --- |
| `python3 <script>.py` | ~60 | `<harness>/validate/preflight_anchor.py`, `<harness>/health/pr_status.py`, `<harness>/landing/rebase_wrapper.py` |
| `*.sh` | ~49 | `<project>/ci/test_harness.sh`, `<project>/validate.sh`, `<harness>/landing/land-pr.sh`, `<harness>/stress/nightly.sh` |
| `make <target>` | 37 targets | a pin-check target → `scripts/<pin-check>.rs` → `with-proxy git fetch` |
| `#!/usr/bin/env rust-script` | 21 | `<harness>/<entrypoint>`, `scripts/<pin-check>.rs`, `compat-envelope/collect-envelope.rs` |

**Roughly 167 wrapped entry points against 3 direct candidates** — one is disabled by default and
one of the two defaults leaks through hooks. Any sweep
that assumes "find the network call, add an allowlist entry" will resolve a small minority of what
it finds. That ratio is the deliverable.

Note `python3 foo.py` puts `python3` in argv[0]. It is not a special case; it is the general case.

### (c) NOT THE RELAY'S PROBLEM

Nested calls where the *outer* invocation already crossed correctly need nothing further — the whole
subtree inherits egress. `git commit` running its hook is category (c) operationally and (a′)
security-wise, which is precisely why the two must be reasoned about separately.

## Only now, the mechanism

Three options were named in the task. Weighed against the classification above:

**1. Teach the relay to match the inner command.** Rejected. It cannot be done soundly. Determining
what `make check` or a rust-script will invoke requires executing it; any static approximation is a
proxy for the real behaviour, and a proxy that governs a security boundary is the failure mode this
repository keeps finding. It also fails open — an unrecognised wrapper looks like "no inner command".

**2. Wrappers exec through the relay explicitly.** Correct where we own the wrapper, and it is what
we have already been doing by hand five times. It scales linearly with components, needs every
wrapper edited, and does nothing for wrappers we do not own. Right for the handful of hot paths;
wrong as the general answer.

**3. Give wrapped tools a proxy-configured environment instead of relay routing.** This is the one
that fits the shape of the problem, and today's evidence points at it directly: `gh` failed with
`network is unreachable` purely because it reads `HTTPS_PROXY` and nothing set it, while `git`
worked because `~/.gitconfig` carried `http.proxy`. Neither tool was broken — the *environment* was
the missing piece. A wrapped invocation that runs with proxy variables set needs no allowlist entry
and no relay hop at all.

**Recommendation: (3) as the default, (2) for hot paths we own, (1) never.** But note what (3)
concedes — it grants egress to the whole wrapped subtree, exactly as (a′) already does for git
hooks. It should therefore be adopted as a *deliberate* widening with its own audit trail, not
presented as a tightening. The honest framing is that we already have that exposure through `git`
and have not been counting it.

**Prerequisite for either (2) or (3): close or consciously accept (a′).** Deciding the mechanism for
167 wrapped entry points while the three-name direct candidate set has an unbounded hook path is
optimising the wrong end.
