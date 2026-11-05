# herdr-agent paired-port adversarial review — 2026-08-09

Three independent repository audits reviewed the newly published interactive-agent command from
package/documentation, runtime/security, and executable-contract perspectives. They found that the
Python distribution exported `herdr-agent` without a Rust binary, source-checkout launcher,
inventory entry, shared guide, dependency smoke, package-resource gate, or behavioral
differential. The command was therefore a public unpaired tool despite the repository's stated
contract. The completion work added an independent native implementation and challenged its
durability and targeting rules rather than hiding the entry point.

## Findings resolved

- The Rust crate now owns a `herdr-agent` binary and library transport. It independently implements
  stable-session resolution, identity assertions, private durable queues, FIFO draining,
  cross-process serialization, idle/done readiness, atomic multiline submission, native working
  confirmation, transcript reads, and typed delivery outcomes.
- Python and Rust use the same queue layout and binding JSON. Exact-pane and stable-session forms
  resolve to one live pane, derive the same SHA-256 lock in a fixed per-UID host directory, then
  revalidate after acquisition. Different implementations and `TMPDIR` values therefore still
  serialize one interactive target.
- If both an exact pane and stable session are supplied, the resolved pane must equal the asserted
  pane. A stale pane can no longer be silently overridden by a valid session lookup.
- Queue roots and state directories must be real, same-user directories. Mutating operations
  tighten an existing same-user queue to `0700`; observational status refuses unsafe modes without
  changing them. Lock and binding files are opened no-follow and must be private owned regular
  files. Message identifiers cannot escape the queue root.
- Prompts remain in `inbox` during readiness and identity checks. Immediately before injection they
  cross a synced `inflight` rename barrier. A restart or any ambiguous post-injection failure moves
  that artifact to `failed` without reinjection; invalid bytes are preserved with separate error
  metadata while later valid FIFO entries continue.
- Installed docs now come from one shared implementation-neutral guide. Both package artifacts
  contain and smoke their command-specific guide; the Python checker lints every shipped document,
  and the Rust checker maps each binary to its packaged guide.
- All three source command surfaces expose the command: direct Python source, Cargo-backed Rust
  source, and the deterministic common resolver. A manifest-derived test requires every published
  console command and Cargo binary to appear in that topology.

## Executable evidence

The black-box differential runs 50 paired checks. Each package invokes its production client
against an isolated executable Herdr protocol fixture. It compares version and guide bytes, help
schema, bare startup, exact-pane and stable-session status, read fallback, literal multiline
submission, durable processed state, busy pending state, ambiguous transport failure, malformed
FIFO quarantine, strict JSON and attempt schemas, exhausted-attempt handling, target contradiction,
and invalid CLI domains, including option-value theft and strict bounded ASCII numeric grammars.
It also passes pending queues in both directions between editions and
holds a real mixed-edition lock race across different target forms and `TMPDIR` values. Machine
JSON, normalized durable artifacts, exit 75/76 meaning, and exact submission counts agree.

The native unit suites additionally cover concurrency within one queue and across distinct queues,
inflight restart recovery, queue binding changes, Unicode lock identity, symlink attacks, legacy
message shape, wrong protocol events, crash points, and fsync structure. Artifact checks build the
wheel, source archive, and registry crate in isolation, install or build them without a sibling
source tree, and smoke `--help`, `--version`, and the embedded command guide.

## Remaining trust boundary

The queue protects against ordinary crashes, concurrent callers, stale identities, and accidental
duplicate delivery. It is not a security boundary against another process running as the same
account, which can usually rewrite owner-controlled state or reach Herdr directly. Enforceable
separation requires an external broker and sandbox policy that deny those raw channels.
