# Per-syscall Detcore pages: schema `syscall-pages/v1` (normative)

One page per entry of the pinned `syscalls` crate's x86-64 `Sysno` table
(373 entries, numbers 0-461) lives at `docs/syscalls/<name>.md`. A page
records what Detcore, Hermit's determinism engine, does with that syscall
and why: the determinization strategy, the Linux behaviour behind it, the
tests that should exist, and a dated log of how the assessment changed.

`scripts/syscall-pages.rs lint` enforces every rule below that has an id.
Where this file and the script disagree, the script is what runs; report
the difference as a defect.

Scope rules:

- These pages track Detcore strategy only. Which backends can run a
  syscall is orthogonal and is not recorded here; there are no per-backend
  fields. Test evidence is judged on the default backend in the strict
  container, and recorded as prose Status entries.
- Which syscalls the test matrix exercises is a coverage measurement, not
  a strategy. It is linked from the generated index and never merged into
  the strategy table.
- `done` is judged under `hermit run --strict` on the default container.
  Modes without that container are reduced fidelity at the user's own
  risk; a page may note the difference in one `Reduced fidelity:` line
  under `## Hermit design`, but no flag item, test, or done judgement
  depends on them.

## Page layout

Every page is YAML front matter followed by exactly these nine level-2
headings, once each, in this order (P02). Nothing may precede the first
heading. Level-3 and deeper headings are free inside a section.

| # | heading | content |
|---|---|---|
| 1 | `## Summary` | What the syscall does and how Hermit handles it today, in prose |
| 2 | `## Linux semantics` | Kernel behaviour that matters for determinism; exactly one `### Flags and options` inventory (P10) |
| 3 | `## Determinism analysis` | Sources of nondeterminism and when the call repeats; justifies `linux_determinism`; a `### Cross-host variation` subsection iff `cross_host_variant` (P29) |
| 4 | `## Hermit design` | What Detcore does today with `file:line` citations at a stated hermit SHA, the intended policy, and why; justifies `target_policy` |
| 5 | `## Recommended tests` | `T<n>` and `U<n>` items (P11), or `_None proposed yet._` while `tracking_status: todo` |
| 6 | `## Done criteria` | What must hold for `done`; the default text is generated from the rule in the Done section below |
| 7 | `## Status` | Dated, append-only log (P12, P13) |
| 8 | `## Discussion` | The one discussion link for this syscall (P34) |
| 9 | `## Additional information` | History, migrated legacy evidence, open questions |

Sections 1-4 and 6 must be non-empty (P03).

## Front matter

```yaml
---
schema: syscall-pages/v1
syscall: read
nr_x86_64: 0
linux_determinism: conditional   # deterministic | conditional | nondeterministic
target_policy: model             # model | emulate | refuse | passthrough | investigate
tracking_status: todo            # todo | in-progress | blocked | done | wontfix
cross_host_variant: false
related: []                      # full GitHub issue/pull URLs only
discussion:
  kind: new                      # existing | new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=...
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: <40-hex hermit SHA>
  detcore_class: determinized    # determinized | passthrough | unsupported
  dispatch: "typed"              # typed | sysno-guard | pred:is_<fn> | passthrough | unsupported | fallback
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---
```

- Exactly these keys; an unknown or missing key fails (P05). The front
  matter is a small fixed YAML subset (scalars, the `related` list, the
  two nested blocks); anything outside that subset is a parse error (P01).
- `syscall` equals the file name and a pinned `Sysno` name (P06);
  `nr_x86_64` equals that entry's number (P07, and the facts check).
- Judgement keys (`linux_determinism`, `target_policy`, `tracking_status`,
  `cross_host_variant`, `related`) are agent-maintained. Every change to
  one needs a Status entry in the same commit.
- `code_facts` is tool-owned. Only `facts --apply` writes it. `as_of` is
  the hermit SHA at which the displayed values were last exported with a
  change; pages whose values have not changed keep their earlier SHA, so
  a facts refresh touches only the syscalls whose policy moved. Facts
  come from Detcore's public functions through
  `scripts/syscall-pages-facts.rs`, the dispatch parse in
  `scripts/syscall-pages.rs`, and a count of test files naming
  `SYS_<name>`, `__NR_<name>`, or `Sysno::<name>` (advisory; it
  under-counts libc wrapper use).
- `dispatch: fallback` means a Determinized syscall reaches only the
  catch-all of the typed dispatch. `check` fails on it. The dispatch
  parse assigns `typed`, `sysno-guard`, or `pred:is_<fn>` from the
  dispatch in `detcore/src/lib.rs` and the predicates in
  `detcore/src/syscall_classification.rs`; any shape it does not
  understand reports `parser-failed` for every syscall, never a pass
  (P27).
- `cross_host_variant` is true when something the guest observes through
  this syscall under Hermit is stable on one host but differs between
  hosts (filesystem properties, kernel version, CPU features, sysctls,
  security modules). It is a tag, not a promise to virtualize. It is true
  iff `## Determinism analysis` holds one `### Cross-host variation`
  subsection with at least one item naming the host property (P29).

The strategy shown in `syscalls.json` and the index is derived from code
facts only: `determinized` (modelled or emulated), `determinized-refused`
(determinized with a fixed errno), `passthrough`, or `unsupported`.

## Flags inventory and recommended tests

`### Flags and options` lists every flag, mode, option, multiplexed
operation, and argument-dependent behaviour, one `- F<n>: <behaviour>`
item per behaviour (continuations indented two spaces). The only other
allowed content is the sentinel `_None: the syscall takes no flags or
options._`. A repeated id, an empty inventory without the sentinel, or a
stray line fails (P10).

`## Recommended tests` items name the flags they cover:

```text
- T1 (covers F1, F3): <behaviour to exercise>
  - sub-cases are indented
- U1 (covers F13; host-config): <why CI cannot produce it>, <path>:<line>
```

A repeated id, a covered id the inventory does not define, or an item
without a `covers` list fails (P11).

Error handling follows the rule that an errno the guest sees must depend
only on deterministic conditions: the guest's inputs, the container's
virtual state, and host configuration fixed for the run. Errors decided
by the guest or container are tested with T items. An error decided by
the host's configuration is a `host-config` U item: it requires
`cross_host_variant: true` (P32), cites the `path:line` where Hermit
forwards the error (P11), and stands in for a test under Done. An error
decided by transient host state is a `host-transient` U item: Hermit
must hide it or report it as an infrastructure error before it counts.

## Status entries

```text
- 2026-11-15 · hermit 0123456789ab · agent-name · <text>
```

One bullet per entry (P12), at least one entry, dates never decrease
(P13), and entries are append-only: editing or deleting a past entry is
a review defect, and reordering is caught by P13 where dates differ.

Coverage is recorded as `T<n> covered by <path>` inside an entry whose
text starts with `covered:` or `done:`; the same words anywhere else do
not count, and a later `uncovered:` entry withdraws the T ids it names.
Lint checks that the T id is defined and the path exists in the tree
(P33).

Not yet enforced here: the predecessor system's rule that a coverage
path must be a test that runs the hermit binary on the default backend
in the strict container with a guest that makes the syscall. Until that
check is ported, a coverage token is an assertion by its author, and a
reviewer checks the guest source. A `done` claim must not be read as
machine-verified test coverage beyond P33.

## Done and wontfix

`done` requires all of:

- P14: code facts realise `target_policy` (`refuse` needs a refusal;
  `model` and `emulate` need determinized and not refused; `passthrough`
  needs passthrough; `investigate` is never done).
- P16: the last Status entry starts with `done:` and records evidence.
- P17: every F id is covered by a T item with a recorded coverage token,
  or by a `host-config` U item; there is at least one coverage token; no
  token names an undefined T id.
- P33: every coverage path resolves.

`wontfix` requires the last Status entry to start with `wontfix:` (P18).
If a problem is later found, `tracking_status` returns to `todo` or
`in-progress` with a Status entry saying why.

## Discussion

Every page carries one discussion link (P34). When a discussion issue
exists on `facebookexperimental/hermit`, `discussion.kind` is `existing`
and the URL links it. Otherwise `kind` is `new` and the URL is a
prefilled new-issue form on that repository, titled
`Syscall discussion: <name>` and linking the page, so no issue exists
until a human submits the form. `## Discussion` must link the same URL.
Tooling never creates issues.

## Public text (P30, P31)

These pages are public. No host names, home or scratch paths, internal
diff or task identifiers, or internal URLs anywhere in a page, in
`syscalls.json`, or in generated site output (P30). Legacy measurement
evidence names the kernel and hardware class ("the measurement host,
Linux 6.17, x86_64"), never the machine. The dropped v1 key
`ptrace_verification` may appear only inside historical Status entries
(P31).

## Generated files and tool commands

`docs/syscalls/syscalls.json` and the table in `docs/syscalls/README.md`
are generated by `scripts/syscall-pages.rs aggregate` from the pages and
must not be hand-edited; `check` and the Makefile lint target fail when
they are stale.

| command | effect |
|---|---|
| `lint` | schema, sections, inventories, Status, discussion, scrub; no build |
| `facts` | print code-facts differences against a fresh export at HEAD |
| `facts --apply` | rewrite changed `code_facts` blocks and append a Status entry per changed page |
| `aggregate` | regenerate `syscalls.json` and `README.md` |
| `check` | lint + aggregate freshness + facts equality + dispatch agreement |
| `check --quick` | the same without building the facts exporter; proves self-consistency and dispatch-arm presence, not that claimed classes are today's |
| `convert` | the one-time import used to create these pages from the recovered v1 bodies |

## Rule index

| id | kind | rule |
|---|---|---|
| P01 | error | front matter parses as the fixed subset |
| P02 | error | the nine level-2 headings, once, in order, nothing before the first |
| P03 | error | Summary, Linux semantics, Determinism analysis, Hermit design, Done criteria non-empty |
| P05 | error | exactly the schema keys; `schema` is `syscall-pages/v1` |
| P06 | error | `syscall` matches the file name |
| P07 | error | `nr_x86_64` inside the pinned range (exact value pinned by the facts check) |
| P08 | error | judgement values inside their enums; `related` holds GitHub URLs |
| P09 | error | code-fact values inside their enums; `as_of` is 40-hex |
| P10 | error | flags inventory shape |
| P11 | error | recommended-tests shape, defined cover ids, U-item citations |
| P12 | error | Status entry format; at least one entry |
| P13 | error | Status dates non-decreasing |
| P14 | error | done realises `target_policy` |
| P16 | error | done needs a `done:` last entry |
| P17 | error | done needs every flag covered and at least one token |
| P18 | error | wontfix needs a `wontfix:` last entry |
| P27 | error | code facts equal the export and dispatch parse at HEAD (`check`) |
| P29 | error | `cross_host_variant` iff one Cross-host variation subsection with items |
| P30 | error | no internal or host-specific text |
| P31 | error | `ptrace_verification` only inside historical Status entries |
| P32 | error | a host-config U item requires `cross_host_variant: true` |
| P33 | error | coverage tokens name defined tests and existing paths |
| P34 | error | discussion kind/URL shape and section link |
| W01 | warning | stale issue-era text |
| W02 | warning | nondeterministic under Linux but passed through while not blocked |
