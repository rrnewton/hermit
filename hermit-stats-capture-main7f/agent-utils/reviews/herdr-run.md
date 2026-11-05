# herdr-run paired-port adversarial review — 2026-08-06

Independent implementation and security reviews challenged the Python and Rust packages after the
initial port. The review covered command admission, executable provenance, Herdr protocol
narrowing, workspace/tab lifecycle, concurrency, readiness, byte capture, audit behavior,
configuration parsing, package isolation, and installer-facing documentation.

## Outcome

The review found security-relevant overclaims and destructive edge cases that made the initial
port unsuitable to publish. Most importantly, default `cargo fetch` was not a no-code-execution
capability: Cargo can invoke repository-configured compiler wrappers even for dependency-oriented
commands. Cargo was removed from the default allowlist and retained only as an explicitly trusted,
fail-closed project widening. The package now accurately describes its allowlist as a cooperative
safety rail rather than a security boundary against another process running as the same user. All
high-priority findings below were fixed in both implementations before this report was accepted.

## Findings resolved

- Project-local pane locks were replaced by an account-global namespace rooted at the current
  account database home. Both ports derive the same lowercase SHA-256 pane lock name, so callers
  from different projects serialize against the same Herdr pane.
- Workspace/tab discovery and creation now run under one account-global resolution lock and repeat
  live lookup while holding it. Concurrent first runs cannot create duplicate labels.
- Tool-created spool, cache, run, and lock directories use `0700`; command, output, metadata,
  cache, and lock files use `0600`. A pre-existing configured spool directory is never chmoded,
  including `spool_dir: .`, and final audit/spool symlinks are not followed. The shell wrapper also
  sets `umask 077`.
- Every Herdr/systemd control subprocess has an explicit bound. Ordinary control calls use 30
  seconds; an interactive-agent working wait uses its requested timeout plus a small shutdown
  margin. Both process transports launch a fresh process group and kill the group on timeout;
  Python reaps and drains its direct child, and Rust drains both pipes concurrently before
  reaping. Invalid subprocess bytes are decoded with replacement in both ports.
- Command/readiness timeouts have one shared one-year maximum at configuration, CLI, and public
  library boundaries. NaN, infinity, negative, and huge finite values can no longer become an
  infinite wait or differ between ports. A timeout preserves partial output and the spool path.
- Configuration parsing is strict YAML 1.2 core behavior in both ports: duplicate/merge/custom-tag
  inputs, wrong shapes, non-finite and excessive numbers, control characters, surrogates, invalid
  UTF-8, and unknown keys fail as typed configuration errors without a traceback or panic.
- Protocol responses require the documented list/object/string/integer shapes. Process IDs must be
  positive 32-bit `pid_t` values, embedded-NUL identifiers become typed failures, pane identity and
  pane/tab/workspace relationships are checked, duplicate labels fail closed, and cached ids are
  revalidated through unique live label resolution. Corrupt or invalid-UTF-8 caches are ignored.
- Executable selection ignores caller `PATH` and `HOME`, uses the same fixed candidate order in both
  ports, canonicalizes each candidate, and rejects group/world-writable executables. The same
  validation applies to `systemd-run`.
- CLI differences found by review were pinned and removed: Python option abbreviations are off,
  Rust stops interpreting options after `--`, option values cannot be stolen by help/version,
  loose command words are refused rather than silently rejoined, explicit-agent wrapper words are
  preserved, environment-derived agent names are trimmed, and the default command directory is
  the caller's current directory. CWD and doctor failures now have matching typed/audited outcomes.
- Raw mode emits each immutable captured byte stream once. JSON mode supplies both
  replacement-decoded text and byte-exact base64. Spool-read and output-write failures are typed;
  metadata failure after completion warns and yields `"meta": null` instead of hiding the
  command's output or status.
- Audit append failure is visible but best effort. The audit file is opened no-follow and an
  existing parent is never chmoded. A normal execution records admission before target
  resolution/launch and a final outcome afterward; refusals, dry runs, wrapper failures, and doctor
  probes are represented. Documentation no longer claims durable or tamper-proof logging.
- Retention prunes only a completed run with a regular, parseable `exit_code`, aged from that
  marker's mtime. Active/timed-out runs, FIFO or symlink markers, unsafe root/ancestor symlinks,
  invalid negative/huge windows, siblings, and loose files fail closed toward preserving evidence.
- Cargo is not allowed by default. An explicit project opt-in must retain a positive Cargo
  subcommand list; the minimum `--config`/`-Z` injection guards cannot be removed by replacing a
  policy map, and attached or post-subcommand forms are refused. Documentation states that even an
  admitted fetch may execute ambient helpers and is therefore a deliberate trust widening.
- The fake Herdr implementation moved out of the Python distribution into tests. Python and Rust
  package artifacts contain only their own implementation and standalone language-specific docs.
- Process-reaping evidence now requires the recorded boot to equal the live boot even when a pane
  shell is already absent. Boolean/malformed process identities and conflicting shell incarnations
  for one pane yield unknown rather than reusing the first convenient identity as stale evidence.

## Executable evidence

The black-box `herdr-run` differential currently exercises 106 paired cases covering startup/help/version and
argument parsing, command tokenization/rendering and policy refusal, successful and malformed
configuration, option placement, dry-run JSON/text output, audit JSONL, control-character classes,
Unicode, Cargo opt-in guards, and stable exit behavior. The repository contract runs it through
`make cross`.

Lifecycle behavior is also pinned independently in both unit suites: account-global first-create
and pane-lock concurrency, cache validation, split-pane refusal, readiness, control-process timeout,
private filesystem modes, byte capture, collision-safe spools, and metadata/audit degradation.
Package checks build and install each wheel/sdist and crate archive in isolation before exercising
its installed commands and embedded guides. The follow-up completion run passed 111 Rust crate
tests, strict Clippy, all 106 `herdr-run` paired cases, and all 50 `herdr-agent` paired cases.

## Deliberate limitations

The differential does not inject a fake executable through `PATH`: production resolution rejects
that override by design, and a test hook in the shipped command would weaken the provenance rule it
is meant to test. Therefore live Herdr lifecycle/protocol/concurrency parity is evidenced by
corresponding deterministic unit scenarios, not by the 106 black-box cases. A future external test
environment with an isolated account and real Herdr server can add end-to-end differential coverage
without adding a production bypass.

The default `git` and `gh` policy still grants the capabilities of those programs, including
repository hooks and ambient user configuration. Targeted deny rules remove common escape hatches,
but they are defense in depth rather than a positive capability model. Projects should narrow
`allow` and the deny maps for their use case.

Audit records are private operational evidence, not durable security logs: they are not fsynced,
replicated, immutable, or protected from the same UID. Enforceable policy and audit require a
separate broker plus sandbox rules that deny direct Herdr and user-systemd access.
