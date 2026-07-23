# Test orchestration scripts

## `test-suite.sh` — the single source of truth for the test matrix

Local validation and CI used to list their tests independently and drifted apart
(e.g. the fail-closed ratchet, debugger, LevelDB and backend-parity checks ran
only in CI). `test-suite.sh` fixes that: it defines the test matrix **once**, as
named *tiers*, and both entry points call it.

```
        validate.sh ──┐
                      ├──> scripts/test-suite.sh
   .github/workflows/ │         (defines every tier once)
        ci.yml ───────┘
```

### Tiers

Run `./scripts/test-suite.sh --list` to see every tier and what it does. Each
tier is a small function wrapping the exact `cargo`/script commands for one
concern (build, clippy, fmt, unit tests, the determinism suites, the fail-closed
ratchet, debugger tests, rr suite, backend parity, …).

Every tier is tagged with the machine capability it needs:

| capability   | meaning                                              |
| ------------ | ---------------------------------------------------- |
| `-`          | none — runs anywhere (also on GitHub-hosted runners) |
| `pmu`        | hardware performance counters (retired-branch clock) |
| `namespaces` | user/mount namespace support for Hermit containers   |
| `rr`         | the `third-party/rr` submodule is checked out        |

`kvm` and `dbi` are probed *inside* the `backend-parity` tier and reported as
notices rather than gating the whole tier.

### Modes

| invocation                     | used by            | behaviour                                                                 |
| ------------------------------ | ------------------ | ------------------------------------------------------------------------ |
| `test-suite.sh <tier>...`      | ci.yml steps       | run named tiers verbatim; caller owns any gating (ci.yml `if:` guards)    |
| `test-suite.sh --portable`     | CI GitHub-hosted   | tiers needing no special hardware                                        |
| `test-suite.sh --hardware`     | CI self-hosted     | capability-gated tiers; **fails loudly** if a required capability is gone |
| `test-suite.sh --ci`           | full self-hosted   | `--portable` + `--hardware`, fail-loud                                    |
| `test-suite.sh --local`        | (aggregate) local  | everything the host can run; missing capabilities **skip with a notice** |
| `test-suite.sh --quick`        | inner-loop         | build + clippy + fmt + unit-regular + smoke                              |
| `test-suite.sh --list [MODE]`  | humans / validate  | list tiers; `--plain` emits `<fg|bg>\t<tier>` for capability-present tiers |

The fail-loud-in-CI / skip-with-notice-locally split follows the repository's
"fail loudly instead of skipping tests" policy: CI must never silently drop a
test, but a developer laptop without a PMU should still get useful signal.

### How the two entry points consume it

* **`validate.sh`** asks `test-suite.sh --list local --plain` for the tiers this
  host can run and drives each one through its own logging/parallelism harness,
  so per-check output is preserved. Capability-absent tiers are announced and
  skipped.
* **`ci.yml`** calls individual tiers per step (`./scripts/test-suite.sh build`,
  `… ratchet`, …), keeping GitHub's step names, the JUnit upload, and the
  mount-namespace gate while sourcing the actual commands from here.

### Adding or changing a test

Edit the tier function (and, if new, the `ts_registry` table) in
`test-suite.sh`. Both `validate.sh` and CI pick the change up automatically —
there is no second list to update.

## `test-fail-closed.sh`

The unsupported-syscall fail-closed ratchet, invoked by the `ratchet` tier. See
the header comment in that file and `hermit-cli/tests/fail_closed_*.tsv`.
