# Hermit Users Guide

**Hermit is a a program sandbox for repeatability & concurrency testing.**

Hermit, by the Hermetic Infra Team, launches programs in a special sandbox to control their execution. Hermit translates normal, nondeterministic Linux behavior, into deterministic, repeatable behavior.  This can be used for various applications, including:

- record-replay debugging,
- simple reproducibility,
- "chaos mode" to expose concurrency bugs in a controlled and repeatable way.

Hermit is a middleware (or “sentry”) that sits between the guest process and the OS, accepting guest system call requests on one side, and producing sending system calls to the real Linux kernel on the other side.

Hermit currently supports x86_64 Linux and can be run via:

```
~/fbsource/fbcode/hermetic_infra/hermit/hermit
```

And here's a minimal demo of running inside hermit's deterministic environment:
```
$ cd ~/fbsource/fbcode/hermetic_infra/hermit/
$ ./examples/race.sh
bbbbbbaa (... nondeterministic output...)
$ ./hermit run --strict ./examples/race.sh
abababab (... deterministic racing processes...)
```

Strict deterministic execution is the default: `./hermit run ./examples/race.sh`
behaves the same as the `--strict` form above. The `--strict` flag is retained
for command-line compatibility and does not make the default any stricter. See
[USER_GUIDE.md](USER_GUIDE.md) for the full set of mode-selection options.

## Strict verification receipts

Use the complete strict invocation when a result will authorize a compatibility
or determinism claim:

```text
hermit run --strict --verify --verify-strict --verify-json receipt.json -- PROGRAM ARGS...
```

This publishes `receipt.json` only after its raw evidence is present in the
adjacent `receipt.json.artifacts/sha256/` content-addressed store. The versioned
receipt binds the exact Hermit source and executable, guest binary, command/test
identity, effective run configuration, typed termination of both executions,
raw stdout/stderr, and the complete ordered INFO stream. The INFO framing removes
only the declared real wall-clock prefix and ordinalizes only explicitly marked
host addresses; numeric values, hex values, paths, COMMITs, and DETLOGs remain
exact. Add both `--detlog-heap --detlog-stack` for an L3 receipt; requested
memory classes must each be nonempty on both runs.

The historical top-level `verified` and `bitwise_parity` fields remain present
for diagnostics, but are not an authority. A consumer must call
`hermit::verify_receipt::load_and_verify_strict_receipt` with its independently
expected source/binary/test/config identity. The verifier dereferences every
blob, re-parses the raw logs, recomputes message classes and digests, and returns
a typed `qualified`, `diverged`, or `no_result` decision.

Strict record/replay uses the same authority rather than the legacy boolean:

```text
hermit record start --verify --verify-strict --verify-json receipt.json -- PROGRAM ARGS...
hermit record start --verify-receipt receipt.json \
  --expected-source-revision FULL_40_HEX_SHA -- PROGRAM ARGS...
```

The second command is the consumer: it independently rebuilds the expected
producer, guest, command, backend, and effective record configuration before
calling the shared semantic verifier. A bare `bitwise_parity: true` document is
therefore a no-result. SaBRe receipts retain its untouched multiplexed stderr
transport before DETLOG extraction, preserving the original ordering within
that stream. They remain a typed no-result until the backend has a lossless
ordering transport between those events and the coordinator log.

## Further reading

* Find hermit [CLI examples here](https://fb.workplace.com/notes/hermetic-infra-fyi/hermit-tech-preview-a-linux-reproducibility-tool/244656753248444/)
* See demos in [this tech talk](https://fb.workplace.com/groups/591973351138875/permalink/1132872253715646/).
* [This talk](https://fb.workplace.com/groups/591973351138875/posts/1533285603674307) shows concurrency testing with hermit "chaos" mode.
* [This talk](https://fb.workplace.com/groups/hermit.fyi/post
