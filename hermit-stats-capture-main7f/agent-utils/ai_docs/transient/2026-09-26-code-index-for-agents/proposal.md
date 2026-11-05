# Code navigation for agents: Glean, semcode, language servers, and what to build

*2026-09-26. Working proposal, not settled documentation. Slug: `code-index-for-agents`. Nothing
described here as `codenav` exists yet; it is a proposed tool.*

## What was asked

> I want agents to have a faster and more token efficient way to read code when they work with this
> code base. Let's focus on the Rust side tools first. Let's research other projects experience with
> glean, with semcode, and with related systems. An agent should be able to take a function in a
> file, and e.g., enumerate its knows callers efficiently. I don't know if this is usually done with
> CLI or MCP. I know that there is some work to get agents to use the same LSP that VScode support
> does. [...] Even though we index Rust code first also consider what we will do for the python code
> in this repo too. But I want to know the recommendation for Rust-only projects in general (as well
> as the blended recommendation for this repo overall).

## The answer, up front

**For Rust-only projects in general:** get precision from **rust-analyzer**, the server VS Code's
Rust extension runs.

What was measured:

- Its live call hierarchy found the callers of all 8 target functions measured here exactly: 211 of
  211 call sites, no false positives.
- Its batch export, `rust-analyzer scip`, found the same sites. Its only extras were the `use` lines,
  which a query layer filters out.
- Glean indexes Rust only through that same export, so it can be no more precise.
- Tree-sitter tools, semcode included, match names. They miss method calls, and method calls are
  most of idiomatic Rust.

The caveat on "exact": 176 of the 211 sites belong to four distinctively named functions, where
`rg -w` did nearly as well. The results that separate the tools rest on the other 25 sites. The
targets did not exercise rust-analyzer's documented blind spots (below).

**What to do with it:**

- Put rust-analyzer behind a small, name-addressed tool that prints one caller per line.
- Keep a long-lived server for the edit loop. For large workspaces, or many concurrent agents, add a
  per-commit `rust-analyzer scip` snapshot.
- Keep `rg` as the fallback.
- Do not adopt Glean unless you already run it at multi-repository scale.
- Do not use semcode for Rust callers. It is a C/kernel tool.

**Without writing any code**, there are three ways to get there today, each with measured caveats:

- (a) Claude Code's built-in LSP tool with a custom rust-analyzer plugin;
- (b) Serena over MCP;
- (c) [crux](https://github.com/pedr0v/crux) over a SCIP index.

**For this repo (Rust plus Python):** one proposed navigation command, `codenav`, with two backends:

- **rust-analyzer** for `rs/`, pinned to `rs/Cargo.toml` so it never loads `vibe-talk/`.
- **ty** (Astral's Python language server) for `py/`, `cross/` and `scripts/`, with basedpyright as
  the fallback.

It should answer `callers`, `refs` and `def` by name. It should label its rows: calls, value uses,
trait-level rows, and a capped handful of text-only matches. It should walk up from Rust impl methods
to their trait declaration automatically. For Python, walk-up works only for nominal base classes;
structural Protocols need the tool's own lookup.

It needs one repo-specific extra, `twin`, which maps a Rust item to its Python counterpart and back,
because many tools here are paired implementations with parallel module paths.

**Build nothing until an A/B shows it pays.** First, configure Claude Code's native LSP tool (phase
0). Then run a small A/B on real "who calls X" tasks: grep only, the native tool, and a `codenav`
prototype. The one task-level study found that LSP improved precision but cost more tokens than grep
(details below).

**CLI or MCP?** Today it is usually **not** a CLI. LSP-precise navigation reaches agents mostly
through harness-built-in tools (Claude Code's LSP tool, OpenCode behind a flag, Zed, VS Code
Copilot's usages tool) or through MCP bridges, where Serena dominates. semcode's main interface is
MCP too. Where CLIs exist, they are usually a second face of the same binary.

The transport turned out to matter less than two other things:

- **Result format and per-session startup.** The only controlled comparison found MCP and CLI
  "truly a wash", and harnesses now load MCP schemas on demand. Over the 8 Rust targets,
  rust-analyzer's raw LSP JSON came to about 20k tokens, against 3.7k as `path:line enclosing_fn`
  rows. But Claude Code's native tool already prints compact text, and for distinctive names
  compact rows beat plain `rg -n` output by only 10–20%.
- **Precision on common names.** That is where the real saving is, along with fewer follow-up file
  reads.

So: build one engine with two faces:

- **A CLI**, found through a skill plus a one-line AGENTS.md note.
- **A thin MCP adapter that returns the same rows.** This is not optional. Codex has no built-in LSP
  tool ([codex#8745](https://github.com/openai/codex/issues/8745)), so MCP is how a Codex agent gets
  rust-analyzer's precision without going through the shell. `agentctl mcp` already shows the pattern
  in this repo.

Vanilla MCP over rust-analyzer is a sound design. The objection above is to today's off-the-shelf
bridges: their tool-list size, 0-based lines, missing call hierarchy, and bare-name lookup. The
objection is not to MCP.

## How this was evaluated

- **Eight research dossiers**, each re-checked by an independent fact-checking agent against primary
  sources: Glean; semcode; LSP bridges; precise index formats; agent-native tools; CLI versus MCP;
  Python code intelligence; and Rust caller semantics.
  - The fact-checkers confirmed about 190 claims, corrected 47 and refuted 5. The corrections are
    applied below.
  - 12 claims stayed unverifiable, for mixed reasons: GitHub API access was blocked for some
    fact-checkers during the run, some primary pages were unreachable, and some experiments were
    not re-run. Those are flagged where they matter.
- **Three hands-on measurement runs on this repo** at commit `9383f6a`, using `git archive` snapshots
  and never the live checkout. `vibe-talk/` was excluded throughout.
  - Rust: 8 target functions in `rs/`, 211 call sites.
  - Python: 5 targets, 120 hand-verified call sites.
  - Agent interfaces: the token cost of one callers query under each interface.
- **Ground truth.** Every word-boundary `rg` hit was classified by hand, and rust-analyzer's
  resolution was used to separate same-named items. Calls with no callee name at the call site
  appear in neither grep nor rust-analyzer, so they are absent from the ground truth. That covers
  `macro_rules!` expansions, `Drop`, operators, `Display` via `format!`, `?`/`From`, and renamed
  imports. This evaluation cannot detect rust-analyzer missing calls of that kind. The non-LSP graph
  tools were scored against rust-analyzer's own SCIP output.
- **The host** has 316 cores and about 1 TB of RAM. Its load average was 28–44 during the runs,
  because the research agents ran concurrently. Treat timings as indicative.
  - Rust startup was never measured on a small machine.
  - The Python servers were also run pinned to 8 CPUs. Their cold-start times barely changed.
- **Glean** was evaluated from its public source and documentation. Its precision was checked by
  loading this repo's SCIP data with the open-source `scip-to-glean` converter and schema (Glean
  commit `6124f3f`). No Glean build was timed for this evaluation, so this document gives no Glean
  latency or memory figures.

Tool versions: rust-analyzer 0.3.3057 (run with a toolchain that has `rust-src`); semcode at
`facebookexperimental/semcode@95dd3d1` (2026-09-23); ty 0.0.84; basedpyright 1.40.1 (pyright
1.1.414); pyrefly 0.63.1 and 1.3.1; Serena 1.7.0; mcp-language-server 0.1.1; ripgrep 15.2.0;
Universal Ctags 6.0.0.

## What "known callers" means

"Callers" is not one well-defined set in either language. It depends on these cases.

| Case | grep | tree-sitter (semcode, tags, ast-grep) | rust-analyzer live (LSP) | rust-analyzer SCIP (also Glean, Sourcegraph) |
|---|---|---|---|---|
| bare `f()` | yes | yes | yes | yes |
| path call `Type::f()`, `m::f()`, `Self::f()` | yes, by text (exact when the query is path-qualified) | **no** | yes | yes |
| common names (in `rs/`, `new` has 89 definitions, `run` 40) | floods | floods or refuses | exact | exact, except items that share a symbol (below) |
| method call `x.m()` | name only | **no** | exact | exact |
| trait method via generic bound or `dyn` | name only | no | only when the **trait declaration** is queried | lands on the trait symbol only |
| callee named inside macro arguments (`assert_eq!(f(x), …)`) | yes | **no** (token trees are opaque) | yes | yes |
| function passed as a value (`.and_then(parse_size)`) | with `-w` | no | yes (incomingCalls includes it) | yes (as a reference) |
| call written inside a `macro_rules!` body | text | no | no | no |
| renamed function import (`use m::f as g; g()`) | no | no | **no** | yes |
| operators | no | no | no | yes |
| `?`/`From`, `Drop`, `Display` via `format!` | no | no | no | no |
| serde string paths (`#[serde(default = "f")]`) | yes | no | no | no |
| static/const initializers, cfg-inactive code | yes | yes | call hierarchy no; references yes | no |

**Trait dispatch matters most in practice.** `rs/` has roughly 180–300 trait impl blocks (the count
depends on the regex) and about 207 lines with `dyn`. A rust-analyzer maintainer declined
impl-to-trait expansion as not fixable
([rust-analyzer#19358](https://github.com/rust-lang/rust-analyzer/issues/19358)). So querying an impl
method whose callers all go through `dyn` returns **zero callers**. A tool must first go to the
declaration and query that; `textDocument/declaration` on an impl method jumps to the trait item. For
a trait with several impls, a trait-level caller may dispatch to any of them, so those rows are an
over-approximation for any one impl. The repo has `ChatSubscriptionBackend` with 20 impl lines,
`CgroupManager` 11 and `HerdrApi` 10.

**Standard-library traits are a dead end.** `rs/` has 46 `impl Drop`, 30 `impl From` and 32 `Display`
impls, and their callers are implicit. Walking up from them is slow and finds nothing:
`FlagValue::from` took about 7 s for 0 callers.

**Python has the same shape, and one extra.** The callers of a Protocol method appear only when the
Protocol declaration is queried. No server can find a *structural* Protocol from a class that
satisfies it without naming it. This repo defines 21 Protocol classes.

**The SCIP column has structural caveats:**
- Symbols are per package, so same-named items in different test targets merge. rust-analyzer
  printed 116 "Duplicate symbol" warnings on `rs/`.
- Imports are not marked as imports, so they appear as references.
- There are no trait-to-impl relationships
  ([rust-analyzer#14031](https://github.com/rust-lang/rust-analyzer/issues/14031)).
- It reports errors for derive-generated items.

## Measured on this repo

### Rust: 8 targets, 211 true call sites

The targets:
- `canonical_sha256`: unique name; 15 of its 32 caller functions call it only inside `assert_eq!` or
  `json!`
- `ManagedAgents::send`: a common method name on a generic impl
- `SyncBackend::publish`: a trait method with 3 impls, called only through `dyn`
- `read_document::<T>`: generic, with turbofish calls
- `herdr_run::sweep::sweep`: shares its name with 3 private dagrun functions
- `parse_size`: also passed as a value, and called inside closures and macros
- `ChatSubscription::commit_durable`: called from 4 crates
- `dag_from_json`: 85 call sites

| Tool | True / false positives / misses | Output for all 8 | Time per query | Setup |
|---|---|---|---|---|
| **rust-analyzer LSP** call hierarchy | **211 / 0 / 0** (`publish` only when queried at the trait declaration; each impl returns nothing) | 80.7 KB raw JSON; **14.8 KB** as `path:line enclosing_fn` | 0.03–1.3 s for the first query of a symbol, 1–6 ms repeated | ready in 3.2–3.5 s (warm `target/`) or 8.9–9.4 s cold; about 1.7 GB |
| rust-analyzer SCIP + a small SCIP-to-SQLite query script | 211 / 17 / 0 (the false positives are `use` lines) | 16.6 KB | about 20 ms | 25 s and 1.9 GB to build; 28 MB (3.9 MB gzipped) |
| `rg`, the query an agent would naturally type (`name\(`) | 202 / **190** / 9 | 36.7 KB, no enclosing function | 7–9 ms | none |
| semcode 0.1.1 (function level, 150 true caller functions) | 74% recall, 85% precision; **0 on all 3 method targets** | 8.0 KB, no call-site lines | about 0.2 s | 3 s, 430 MB, 6 MB |
| ctags | definitions only | n/a | 4 ms | 0.08 s |

Glean was checked for precision only, not timed: its generic callers query, run over the same SCIP
data, found the same call sites. That is what you would expect of an index built from rust-analyzer's
output.

Five things stand out:

- **Output size.** The compact rendering of the exact answer was smaller than the natural `rg` output
  for all 8 targets, and it saves the agent opening files to classify hits. The largest single answer
  (85 callers) is about 6.9k tokens as raw JSON and 1.6k compact. Appending the trimmed call-site line
  to each row costs about 1.8x the compact size (27 KB for all 8).
- **Where `rg` holds up and where it fails.** For distinctive names, `rg` was nearly as good. The
  natural `name(` query missed 9 sites: 5 turbofish calls and 4 value uses. `rg -w` finds those, but
  adds import and definition lines. Precision collapses on common names: `.send(` produced 150 false
  positives for 6 real callers.
- **A one-shot rust-analyzer** (spawn, wait, ask once, exit) costs 3.5–5 s and 1.7 GB per question.
  Agents need a server that stays up. Memory grows with use: 1.48 GB at ready, 1.71 GB after 8
  queries, and 2.16 GB after one query that spans the whole crate graph.
- **Without `rust-src`**, rust-analyzer took 125–129 s to become ready. It silently dropped calls
  inside std macros (21 references found instead of 44). The host's default stable toolchain lacked
  `rust-src`. The `cargo.sysrootSrc` option can point at any installed std source.
- **Queries sent before rust-analyzer reports it is ready return empty results**, with no error.

### Python: 5 targets, 120 hand-verified call sites

| Server | Call hierarchy | Result | Cold start to first *complete* answer | Memory |
|---|---|---|---|---|
| **ty 0.0.84** | yes | call hierarchy exact on 3 of 5 and best on the other 2 (it follows aliases: 86/88 and 7/8 where the others got 84/88 and 2/8); no false positives; references already exclude imports | 0.5–1.9 s, and the first answer was already complete | 0.3–0.9 GB |
| basedpyright 1.40.1 (a community fork of Pyright, the open engine behind VS Code's Pylance) | yes | no false positives once warm; duplicates method-call ranges | 3.9–8.1 s; the first references answer is **silently partial** | 0.4–1.4 GB |
| pyrefly 1.3.1 | yes | as good as basedpyright; fastest warm references (3–60 ms) | about 2 s; early answers silently partial | about 1 GB |
| pyrefly 0.63.1 (the build installed on this host) | yes | **misses cross-file callers** (0/88 on one target) | n/a | n/a |
| jedi-language-server | no | references fail for methods (1/14) | 1.4 s | 0.1–0.3 GB |
| zuban 0.10.0 | no | misses calls re-exported through `__init__` (78/88) | 0.7 s | 0.1–0.2 GB |
| semcode (tree-sitter) | "callers" | 0/14 on `Runner.run`, 0/3 on a Protocol method | 8.8 s index | 0.7 GB |
| `rg '\.run\('` | n/a | 14 true among 295 lines (about 5.9k tokens) | 9 ms | none |

For `Runner.run`, ty, basedpyright and pyrefly 1.3.1 each returned exactly the 14 callers in about
1.1 KB of compact text. The naive `rg -w run` produced 3,083 lines.

**Under the union this proposal recommends (call hierarchy plus value references), ty, basedpyright
and pyrefly 1.3.1 all found every call site on these targets.** ty's call-hierarchy edge comes from 7
alias sites. On a separate toy project, ty missed cross-module re-exported aliases that pyrefly
caught. So ty is not chosen for accuracy. It is chosen because its first answer is complete, its
references exclude imports, and it uses the least memory. Its risks are 0.0.x churn and background CPU:
it kept using 0.5–1.5 cores after answering.

Two blind spots were shared by **every** server:

- **Protocol or base-class dispatch.** The 3 callers of `cpu_stats` appear only when the Protocol
  method is queried. At either concrete implementation, every server returns zero. The only thing
  that found them from a concrete class was `rg '\.cpu_stats\('`.
- **Callbacks registered as values.** No call hierarchy reports `set_defaults(handler=_cmd_remove)`.
  Every server's references do.

Configuration matters. With no configuration, basedpyright, ty and pyrefly 1.3.1 missed the 2
callers in `cross/` and `scripts/` that import through runtime `sys.path` edits (5 of 7 found), and
zuban did worse. jedi, oddly, found all 7. A search-path configuration covering `py`, `cross` and
`scripts` fixes it.

### What an agent pays for one callers query

This is the first callers query in a session: tool definitions plus the call plus the result, for
`load_config`, measured as bytes divided by 4.

| Interface | Approximate tokens | Correct? |
|---|---|---|
| CLI through the shell tool, compact rows, plus a 506-byte AGENTS.md note (simulated) | **about 0.3k** | exact |
| Claude Code native LSP tool (reconstructed) | about 0.75k | exact |
| Serena over MCP, tool search on, used with discipline | about 1.4k | exact |
| Serena, following its own instructions ("load all tools" and read the manual) | about 9.7k | exact |
| semcode-mcp, tool search on (off: about 6.3k) | about 1.2k | **wrong** |
| mcp-language-server, tool search on | about 1.8k | 2 wrong references (same-named functions merged) |

Two rows are not live measurements:

- **The CLI row is simulated.** Its rows were rendered from real rust-analyzer output.
- **The native-tool row is reconstructed.** Its tool definition (2,139 B) and output format come from
  the public Claude Code 2.1.283 package, filled in with real rust-analyzer results. No LSP plugin was
  run in a live session.

A thin MCP tool returning the same compact rows would cost roughly the CLI's result bytes plus about
230 tokens of schema, loaded on demand. **The differences in this table come from result format and
fixed overhead, not from the transport.** Each further query costs about 0.7 KB with the CLI, 0.8 KB
with the native tool and 3.4 KB with Serena.

## The options, one by one

### rust-analyzer, live over LSP

- **What it is:** the same binary VS Code runs. incomingCalls is find-references restricted to name
  references inside a `fn` body, grouped by the enclosing function.
- **Measured:** exact on the 8 targets, with the caveats above.
- **Requirements an agent integration must meet:**
  - Wait for readiness (`experimental/serverStatus` reporting quiescent) before cross-file queries.
    In this build, cache priming finished *before* quiescence was reported, the reverse of what the
    measurement client first assumed.
  - Install `rust-src`, or point `cargo.sysrootSrc` at an installed copy.
  - Set `linkedProjects` when the repository root is not the workspace root. Here the root has no
    `Cargo.toml`, and auto-discovery would also load `vibe-talk/`.
  - Turn `checkOnSave` off **and** give rust-analyzer its own `cargo.targetDir`. Even with
    `checkOnSave` off, it runs cargo at startup and on manifest changes to build proc macros and
    build scripts. That took 6.1 s here and competes with the agent's own builds for the target
    directory lock.
  - Set `workspace.symbol.search` to `{kind: all_symbols, limit: <large>}` if names are resolved
    through `workspace/symbol`. With the defaults it searches types only, so `run`, `new` and `load`
    found no functions in `rs/`. With `#` (all symbols), each query hit the 128-result cap before
    ranking, again with no exact-name hits. With `all_symbols`, re-exports come back as extra hits and
    must be collapsed to the definition.
- **There is no persistent cache**
  ([rust-analyzer#4712](https://github.com/rust-lang/rust-analyzer/issues/4712)), so every server
  start is a full re-index.
- **At larger scale:**
  - Seconds for `rs/`.
  - 80.6 s to become ready on the Zed repository, measured on shared CPU
    ([lsp-det notes](https://github.com/tagawa0525/lsp-det/blob/main/docs/research/claude-code-dogfooding.md)).
  - Dioxus reported 2–3 minutes for about 75k lines in March 2025, in the same #4712 thread.
  - Maintainers' stabilised memory figures for users' projects ran from about 2.8 GB to 7 GB.
  - 40–44 GB when an agent bridge enabled cache priming on a large, continuously built workspace
    ([serena#1556](https://github.com/oraios/serena/issues/1556)).
- **No CLI route:** rust-analyzer's command line has no callers query. Its structural-search command
  failed to resolve paths in this workspace.
- **Library use:** the `ra_ap_*` crates are 0.0.x, published weekly, and consumers pin exact
  versions. Using them as a library is possible but unstable; the LSP is the stable interface.

### rust-analyzer, offline: `rust-analyzer scip` plus a small query layer

- **What it is:** SCIP ([scip-code/scip](https://github.com/scip-code/scip)) is an interchange
  format. It records occurrences with roles and the enclosing ranges of definitions, not call edges.
  Its own design document says efficient navigation is not its job, so a query layer is always
  needed.
- **The derivation:** the callers of S are the innermost enclosing function definitions of S's
  non-definition occurrences. On reference occurrences rust-analyzer sets `enclosing_range`
  incorrectly; only definition ranges are safe. The layer must also:
  - join trait declarations to impls by symbol grammar (heuristic, because some impl symbols have no
    module path);
  - filter `use` lines;
  - flag the duplicate-symbol classes.
- **Output semantics:** the output is "referencing functions", not labelled calls. rust-analyzer's
  SCIP marks only definitions, so calls cannot be told apart from value uses.
- **It resolves more in some places:** it resolves each token directly, so it also catches operator
  calls, renamed-import calls, and build scripts that include library modules via `#[path]`.
- **Measured here:** 25–31 s and 1.9 GB for `rs/`, 28 MB. Callers matched the live server apart from
  the import lines.
- **Scale:** generation is largely single-threaded. mozilla-central took about 20 minutes
  ([rust-analyzer#18140](https://github.com/rust-lang/rust-analyzer/issues/18140)).
- **Where it fits:** per-commit snapshots shared across worktrees at the same commit, CI, and many
  concurrent agents that should not each run a 1.7–2.2 GB server. It is stale for uncommitted edits.
- **Existing tools:**
  - [crux](https://github.com/pedr0v/crux) is an MCP server that derives callers this way. It is
    weeks old, and its benchmark is self-reported and on Python repos.
  - [scipq](https://github.com/elodhorvath/scipq) is a CLI with an embedded agent skill. It lists
    reference sites, not enclosing callers.

### Claude Code's native LSP tool

- **What it is:** Claude Code has had a built-in `LSP` tool since 2.0.74 (2025-12-19). It offers 9
  operations addressed by file, line and column, including `prepareCallHierarchy`,
  `incomingCalls`, `outgoingCalls`, `findReferences` and `goToImplementation`. The tool definition is
  about 2.1 KB and its outputs are already compact.
- **Getting a server:** the official marketplace
  ([anthropics/claude-plugins-official](https://github.com/anthropics/claude-plugins-official)) ships
  `rust-analyzer-lsp` and `pyright-lsp`.
  - Each plugin is only configuration (`{command: "rust-analyzer", extensionToLanguage: {".rs":
    "rust"}}`). You install the binary yourself so it is on `PATH`.
  - The official pyright plugin launches `pyright-langserver`, so basedpyright's differently named
    binary does not satisfy it.
  - As of 2.1.283, a custom plugin's `.lsp.json` can pass `initializationOptions` and `settings`
    (`workspace/configuration` is answered). That is how `linkedProjects`, `checkOnSave`,
    `targetDir` and `workspace.symbol.search` get set; the official plugin sets none of them.
  - `diagnostics` defaults to true, which pushes diagnostics into context after every edit. That is
    a token cost; set it deliberately.
- **Caveats found:**
  - Nothing in the tool appears to wait for rust-analyzer to be ready (inferred from the package).
    Early `findReferences` calls return "No references found... if the LSP server has not fully
    indexed the workspace". Early `incomingCalls` calls return "No call hierarchy item found at this
    position", which gives no hint that indexing is incomplete.
  - Files changed through the shell are not reported to the server. rust-analyzer watches the
    filesystem itself, so it still sees them. Pyright does not, and stays stale after shell edits,
    `git checkout` or formatters.
  - Queries are position-based: the agent must find a column first.
  - Results in gitignored paths are filtered for references, definitions and workspace symbols (not
    call hierarchy). This repo's sources are not ignored.
  - 2.1.280 fixed the tool for background subagents (the default mode). Other subagent types were not
    checked.
  - Each session starts its own server, even when VS Code is already running one.
- **Discoverability:** Serena's
  [client documentation](https://github.com/oraios/serena/blob/main/docs/02-usage/030_clients.md)
  reports that models strongly prefer their built-in tools. An AGENTS.md line saying when to use
  callers helps.
- **Precision:** it forwards to the same rust-analyzer, so it should inherit its precision. That was
  not tested end to end here.
- **Other harnesses:**
  - OpenCode has the same 9 operations behind an experimental flag.
  - VS Code Copilot's usages tool finds a symbol by name plus a quoted line fragment rather than a
    column, a better addressing scheme for agents.
  - Codex has no built-in LSP. Pyright's author, who contributes to Codex, wrote that LSP "was not
    designed for coding agents" ([codex#8745](https://github.com/openai/codex/issues/8745)).
- **Reusing the editor's running server** is the most literal reading of "the same LSP that VS Code
  uses". VS Code extensions can expose the editor's own servers to agents:
  - [BifrostMCP](https://github.com/biegehydra/BifrostMCP) (find usages and call hierarchy; AGPL-3.0);
  - [tjx666/vscode-mcp](https://github.com/tjx666/vscode-mcp) (references, plus a plain CLI);
  - [juehang/vscode-mcp-server](https://github.com/juehang/vscode-mcp-server).

  They work only while the editor is open, and none was measured here.

### MCP bridges to language servers

- **[Serena](https://github.com/oraios/serena)** (about 30k stars): the largest. It finds symbols by
  name and waits for rust-analyzer to be ready. It was precise once `rust-src` was present (8/8,
  15/15, and 1/1 through a generic).
  - Cold start 6.6–12.5 s, warm 0.2–0.5 s.
  - It returns nested JSON with 3-line snippets and **0-based line numbers**, a trap for agents that
    mix its results with grep or file reads.
  - Its tool list is about 6.3k tokens, and its own instructions tell the model to load every tool
    and read an 8.9 KB manual.
  - It has no call-hierarchy tool. Methods need a `find_symbol` round trip to learn their name path.
  - It would run `rustup component add` on the user's toolchain if rust-analyzer is missing.
  - Licence: v1.7.0, measured here, is the last MIT release. Unreleased `main` relicenses the
    application to GPL-3.0-or-later (its SolidLSP layer stays MIT), and the official marketplace
    entry installs unpinned `main`.
- **[mcp-language-server](https://github.com/isaacphi/mcp-language-server)**: small (6 tools, 0.9k
  tokens). It looks symbols up by bare name, so it merged same-named functions. It marks every tool,
  read-only ones included, `destructiveHint: true`.
- **Others** exist: cclsp, mcpls, agent-lsp. The shared-daemon option ra-multiplex is archived on
  GitHub; it moved to Codeberg as lspmux under the EUPL, which was not verified.

### Glean

- **What it is:** [Glean](https://github.com/facebookincubator/Glean) is a fact database (Haskell and
  C++, RocksDB, the Angle query language). It is a platform, not an agent tool.
- **Rust:** indexed only by running `rust-analyzer scip` and converting the output. rust-analyzer's
  SCIP has no trait-to-impl links, so Glean has none for Rust either.
- **Python:** the documented open-source route is scip-python. pyrefly's `--report-glean` output can
  also be loaded by hand, and it carries real call-edge facts (`python.CalleeToCaller`). But the
  `python-pyrefly` indexer command is not compiled into the open-source build, and open-source CI does
  not test that path.
- **Callers queries:** callers are `codemarkup.ReferencingEntity`: references inside a definition's
  enclosing range, filtered to functions and methods. That is an on-demand join whose cost grows with
  the number of references. High fan-in symbols would need precomputed edges or a result limit to fit
  interactive budgets.
- **Precision:** checked over this repo's SCIP data, it matched its rust-analyzer input and kept
  same-named functions apart.
- **Costs, from public sources:**
  - There are no binary releases, and the Docker demo is documented as not working.
  - A source build is Linux-only and needs GHC plus folly, fbthrift and RocksDB.
  - SCIP-based databases have no incremental updates; each change is a full re-index.
  - Each CLI invocation pays process start-up, so an agent integration needs a warm `glean shell` or
    a server.
  - Glean's own static LSP server has no call hierarchy.
- **No Glean MCP server or agent CLI was found**, either in the repository or by web search. Searches
  mostly return servers for an unrelated product also called Glean.
- **Experience elsewhere:**
  - Glean's co-creator, now outside the company that built it, indexed all of Stackage (Haskell). In
    his own benchmark it took 470 s and 0.8 GB against hiedb's 1,021 s and 5.2 GB, with
    find-references in 0.03 s. The benchmark is Haskell-only, and hiedb was run with
    `--skip-types`. He also wrote that Glean "is not the easiest thing in the world to
    build" and asked for it to be made more installable
    ([post 1](https://simonmar.github.io/posts/2025-05-22-Glean-Haskell.html),
    [post 2](https://simonmar.github.io/posts/2025-06-11-Glean-stackage-vscode.html)).
  - An outside code-intelligence project built it on WSL2: a 98-minute first build and a 4.5 GB
    store. It had to rewrite scip-java ranges before they would load, and did not adopt Glean
    because of operating cost
    ([report](https://github.com/FTurleque/minos-code-intelligence/blob/main/docs/history/milestones/m0/RAPPORT_GLEAN_C1.md),
    in French).
  - An outside company maintains its own build and macOS port. A maintainer calls the open-source
    build system "really painful"
    ([Glean#688](https://github.com/facebookincubator/Glean/issues/688)).
  - Asked whether Glean can feed LLM context, the maintainer said yes in principle, while another
    user noted there is no off-the-shelf plugin
    ([Glean#485](https://github.com/facebookincubator/Glean/issues/485)).
  - A community Docker image project and a Flow-facts experiment could not be re-checked.
- **Maintenance:** in September 2026, declining a request for a Perl indexer, a maintainer wrote that
  "the team has shrunk" and new indexers were unlikely, though pull requests would be reviewed
  ([Glean#732](https://github.com/facebookincubator/Glean/issues/732)). The SCIP conversion path this
  repo would use is still active: about 50 commits from about 12 authors in the past year.
- **Verdict:** worth borrowing from, not adopting. Take its language-neutral verbs (definition,
  references, callers, callees), SCIP symbol strings as stable identifiers, and "callers =
  references inside a definition's enclosing range". Revisit if it ships binaries or an agent
  interface, or if cross-repository or cross-revision queries become the need.

### semcode

- **What it is:** [semcode](https://github.com/facebookexperimental/semcode) was built by Chris Mason
  for Linux-kernel AI review, and is now maintained mostly by Rik van Riel. Its crate description is
  "semantic code search for C/C++ codebases". It parses with tree-sitter (C, Rust, Python, Zig),
  stores in LanceDB, and is git-aware: blob-keyed incremental indexing plus an overlay for
  uncommitted edits. It ships an indexer, a query CLI, a 19-tool MCP server and a small LSP server.
- **Experience elsewhere:** every user found is a kernel developer:
  - Mason's [review-prompts](https://github.com/masoncl/review-prompts) and his kres review agent;
  - Chuck Lever's Claude Code
    [semcode skill](https://github.com/chucklever/cel-kdev/blob/HEAD/plugin/cel-kdev/skills/semcode/SKILL.md);
  - BPF CI: a commit titled "ai-code-review.yml: semcode integration", and another that pins a
    semcode revision "to work around Lance 2.2 decode panic".

  Most of this is known from titles only, and the LWN article on LLM patch review could not be read.
  No non-kernel user, and no published accuracy numbers, were found. Its C support goes well beyond
  Rust and Python: function-pointer registrations, ops tables and architecture scoping. Its 2026 Rust
  work typed receivers for kernel ops-table dispatch, not impl or trait methods.
- **How callers work, for Rust and Python:** callers are bare-name matches. Only calls written as
  `f()` become edges. Method calls, path calls, calls in macro arguments and function values are
  dropped. Same-named items in one file collapse; 46 of 89 `fn new` definitions disappeared.
- **Measured:** 74% recall at function level, 0 on method targets, and 0/14 for a Python method. For
  this repo specifically, the paired Rust and Python implementations share names, so one index
  returned mixed-language caller lists under a header naming only the Rust definition.
- **Other issues found:**
  - `grep_functions` line numbers shift down by the length of the preceding doc comment (34 of 166
    matches were wrong in one file).
  - The MCP server re-indexes on startup and refuses queries until it finishes.
  - The navigation tools do not paginate.
- **Verdict:** this verdict is about Rust and Python callers, not semcode's intended use. It is fast
  (3 s to index `rs/`) and thoughtfully agent-facing, but it is not a callers backend for either
  language. Ideas worth copying: explicit ambiguity notes, refusing to answer from a stale index,
  lazy tool discovery, git-blob-keyed incremental indexing, and the working-tree overlay.

### Tree-sitter, ctags and heuristic code graphs

- **Name matchers** (`rg`, ctags, ast-grep, Aider-style tags, tilth) are complete on distinctive names
  and useless on common ones. About a third of Rust function definitions in `rs/` (33% of 5,445) share
  a name with another definition. Path-qualified `rg` (`Type::f`) is exact for associated functions
  called by path; only receiver calls and trait calls truly need types.
- **Tree-sitter treats macro arguments as opaque.** About 17–19% of references to workspace functions
  in `rs/` sit inside macro invocations, so macro-blind tools lose them.
- **Heuristic graph tools** are precise but incomplete on Rust.
  - The best measured was [codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp):
    precision 0.95, recall 0.68, and the only non-LSP tool that saw inside macros. It has an open bug
    that fabricates edges for std-typed receivers.
  - Codanna scored 1.00 / 0.46 and GitNexus 0.92 / 0.35.
  - All three miss most receiver-method calls, and they did better on Python.
- **Several of these installers rewrite agent configuration** or add hooks that intercept Grep/Glob.
  Keep their caches outside the tree.

### Embeddings and semantic search

Tools such as claude-context, ck and grepai answer "where is the code about X", not "who calls X".
They are not relevant to this capability.

### Compiler-grade alternatives

- **CodeQL** has explicit call edges and expands trait dispatch. Rust took 75 s and 15 GB to build,
  and queries take tens of seconds. It missed one `dyn` call that rust-analyzer resolved. Its free
  licence covers only open-source codebases. At most it is an offline cross-check.
- **Kythe** removed its Rust indexer in 2023. **stack-graphs** was archived in 2025. GitHub withdrew
  precise code navigation in 2024. **LSIF** is superseded by SCIP. **rustdoc JSON** has no function
  bodies. **rustc/MIR call-graph tools** need pinned nightlies and are prototypes or abandoned.

### Python servers and indexers

- **ty**: chosen for this repo for the reasons given under the Python measurements. It is pre-1.0:
  call hierarchy arrived in 0.0.41 (2026-05-31) and go-to-implementation in 0.0.64. It does not infer
  return types of unannotated functions, which matters little in a codebase gated by strict mypy, as
  this one is.
- **Pyright or basedpyright**: the conservative choice. VS Code's Pylance is closed and licensed only
  for Microsoft products
  ([Pylance FAQ](https://github.com/microsoft/pylance-release/blob/main/FAQ.md)), so Pyright is what
  "the same LSP as VS Code" means for Python outside VS Code. It needs a settle heuristic, because
  basedpyright sends no progress notifications and its first references answer is silently partial.
  Its references also need import filtering.
- **pyrefly**: pin 1.3.1 or later. In its default lazy mode, early answers are silently partial for
  about 2 s. In lazy-blocking mode, the first references answer was still partial for 3 of 5 targets.
  Requests sent right after opening a file needed a retry on LSP error `-32800` ("canceled due to
  subsequent mutation"). One dangling symlink made it index zero files, silently. Under this repo's
  `[tool.mypy]` configuration it applies a legacy preset that turns off return-type inference. Its
  maintainers have an open proposal for a JSON CLI aimed at agent skills
  ([pyrefly#3148](https://github.com/facebook/pyrefly/issues/3148)).
- **`dmypy suggest --callsites`**: an existing CLI callers query from mypy, which this repo already
  runs
  ([mypy daemon docs](https://github.com/python/mypy/blob/master/docs/source/mypy_daemon.rst)). It
  prints `path:line: (arg types)` and took 2–7 s per warm query. It is marked experimental, and gives
  no enclosing function.
- **scip-python**: the offline route. It does emit definition enclosing ranges, so references can be
  attributed to callers. But it is built on Pyright 1.1.301 from 2023, its last release was in 2025,
  and the stock `scip expt-convert` fails on this repo's index
  ([scip-python#223](https://github.com/sourcegraph/scip-python/issues/223)).
- **Jedi**: silently stops searching after 30 files that contain the name.

## CLI, MCP, or native tool

The evidence:

- **Transport:** the one controlled MCP-versus-CLI benchmark
  ([Zechner, 2025](https://mariozechner.at/posts/2025-08-15-mcp-vs-cli/); terminal control, not code
  navigation) found them "truly a wash" and recommends building the CLI first.
- **Tool design:** Anthropic's tool-design guidance is interface-neutral. It asks for consolidated
  tools, high-signal output, and pagination and truncation with sensible defaults
  ([Writing tools for agents](https://www.anthropic.com/engineering/writing-tools-for-agents)).
- **Schema overhead** was the big 2025 objection to MCP, and it has largely gone. Claude Code and
  Codex now load MCP schemas only when a tool is needed
  ([Claude Code MCP docs](https://code.claude.com/docs/en/mcp);
  [Codex features](https://github.com/openai/codex/blob/main/codex-rs/features/src/lib.rs)).
  - Claude Code caps each tool description at 2,048 characters, warns on MCP results above 10k
    tokens and caps them at 25k.
  - Deferral is off behind a non-first-party API gateway.
  - Some servers undo deferral by telling the model to load everything.
- **Practitioners are split.**
  [Ronacher](https://lucumr.pocoo.org/2025/12/13/skills-vs-mcp/) and
  [Zechner](https://mariozechner.at/posts/2025-11-02-what-if-you-dont-need-mcp/) prefer skills plus
  CLIs. [Willison](https://simonwillison.net/2026/Jul/31/stateless-mcp/) returned to MCP for
  auditability after the stateless 2026-07-28 specification.
- **Output limits:**
  - Claude Code's shell tool shows a successful command's output inline up to roughly 30,000
    characters, then only a file path and a 2,000-character preview.
    ([tools reference](https://code.claude.com/docs/en/tools-reference))
  - Codex truncates each tool output to 10k tokens.
  - A navigation tool's default page must stay well below both.
- **Discovery in this repo:** the top-level `skills/` directory is not auto-discovered by Codex
  (`.agents/skills`) or Claude Code (`.claude/skills`) without an install step. One AGENTS.md line is
  the only zero-registration path.
- **Benefit:** the one task-level study found real limits
  ([lsp-vs-grep study](https://github.com/agentconnect-md/lsp-vs-grep-token-study); Python and
  TypeScript only, single author, preliminary):
  - LSP raised find-all-callers precision from 0.76 to 1.00, but cost about 12–19% more tokens than
    grep for strong models.
  - Its value tracked how noisy grep was.
  - Agents picked the LSP tool unprompted about half the time on reference tasks, and almost never
    when localizing a bug.

  That matches the per-query measurements here: the savings are concentrated on common and colliding
  names, which cover a third of the Rust function definitions in this repo. It is also why the plan
  below is gated on an A/B.

The conclusions:

- **Queries:** put the engine behind a CLI with **name-based** queries (`crate::path::fn`, `fn`,
  `file.rs:line`, or a short id from a previous answer). Never make the agent supply columns.
  Handle ambiguity by printing numbered candidates, not by guessing.
- **Answer shape:**
  - Begin every answer with a header giving the count, the backend, index freshness, and the
    precision class.
  - Print one row per call site: repo-relative path, 1-based line, enclosing function, and a kind
    tag. Offer `--context` to append the trimmed call-site line, and choose the default from the A/B.
  - Page at about 50 rows. Offer `--format tsv|json` for scripts and for differential tests.
  - Exit 0 on "no results" and on "ambiguous". The shell tool treats non-zero exits as failures and
    shows less output.
- **Discovery:** ship a skill whose description triggers on "who calls / callers / where is X used",
  plus one AGENTS.md line. Check in a Claude Code permission allow rule for the command so calls do
  not prompt.
- **MCP:** ship a thin adapter alongside the CLI, returning the same rows, and reuse the flat,
  closed-schema pattern of `agentctl mcp` (`py/agentctl/mcp.py`). Codex has no LSP tool, so for Codex
  this adapter is the precise path. It also serves shell-less clients.
- **Native tools:** enable the native LSP tool where the harness has one. It is a good second path,
  not a substitute.

## Proposal for this repo

### The command

A sketch of the shape, not real output:

```
$ codenav callers dagrun::sizing::parse_size
# 14 call sites in 6 functions · rust-analyzer live · rs/ indexed after last change · exact
rs/dagrun/src/<file>.rs:<line>   <enclosing_fn>        call
rs/dagrun/src/<file>.rs:<line>   <enclosing_fn>        value      .and_then(parse_size)
...
~ <path>:<line>                  (text match)          text       #[serde(... = "parse_size")]
# 2 text matches shown (serde/monkeypatch strings); --text for all
```

Verbs for phase 1: `callers`, `refs`, `def`. Later: `impls`, `twin`, and `callees`.

`callees` was not scored against ground truth. rust-analyzer's outgoing calls miss callees named
inside macro arguments (17–19% of references here). Trait calls point at the declaration. pyrefly
found only 20 of 35 callees in one Python test.

The measurements showed these semantics are necessary:

1. **Walk up before asking, in Rust.** Use `textDocument/declaration` to go from an impl method to the
   trait item. Query both, and label the trait-level rows `via-trait (may dispatch to any of N
   impls)`. Never walk up into core or std traits (`From`, `Into`, `Display`, `Drop`, `Deref`,
   operators). For those, say at once that the callers form an implicit-call class rust-analyzer does
   not resolve, and fall back to labelled text rows.
2. **Walk up in Python, where possible.** Walk up through nominal base classes. For structural
   Protocols no server helps, so the tool must search the repo's Protocol classes for a method with
   the same name (and a compatible signature), and query each match as `protocol?` rows.
3. **Callers are the union of** call-hierarchy rows (`call`) and value references (`value`). The
   latter covers callbacks, `.and_then(f)` and `handler=f`.
4. **Text rows are capped.** Emit `~` rows only for string-literal contexts that name the target
   (serde attribute strings, `monkeypatch.setattr` and `getattr` literals, the qualified `Type::name`
   form). Cap them at about 10 with a count and a `--text` flag. Uncapped, they undo the saving
   exactly where the tool helps most: `rg -w run` over the Python tree was 3,083 lines.
5. **Freshness is part of every answer.** It is not the same thing as git cleanliness (see the next
   section).
6. **`twin`** maps `dagrun::io::dag_from_json` to `dagrun.io.dag_from_json` and back, using the
   parallel module paths of the paired implementations, and verifies that the other side exists.
   Nothing off the shelf does this. It is untested: before building it, measure how many Rust items
   have a same-path Python item, and list the naming exceptions (for example `wrkslotsd` against
   `wrkslots`).

### Keeping answers fresh

Agents edit through the shell and then ask immediately. Rebases, checkouts and formatters touch
files too.

- **rust-analyzer** watches files itself when the client does not register for file-change events,
  so the daemon's client should not register.
- **The Python servers** depend on the client's `workspace/didChangeWatchedFiles`, so the daemon must
  watch the tree and forward changes. ty's behaviour here was not tested.
- **Before every query**, wait for rust-analyzer's quiescence again, because it flips after a change.
  For basedpyright, which emits no progress, use a settle heuristic.
- **Retry pyrefly on `-32800`.**
- **The header reports index freshness**: the last change processed, and whether any file is newer.

### Backends and their configuration

- **Rust:** rust-analyzer configured with:
  - `linkedProjects = ["rs/Cargo.toml"]`, never auto-discovery (which would load `vibe-talk/`);
  - `rust-src` present, or `cargo.sysrootSrc` pointed at one;
  - `checkOnSave` off **and** its own `cargo.targetDir`;
  - `workspace.symbol.search.kind = all_symbols` with a large limit.
- **Python:** ty with a search path covering `py`, `cross` and `scripts`. basedpyright is the fallback
  behind the same interface, with import filtering and a settle heuristic. pyrefly (1.3.1 or later)
  can be re-evaluated once its readiness and symlink behaviour settle. Whether that search-path
  configuration is checked in at the repo root (a new path for `scripts/validate.py` to classify) or
  lives in `codenav`'s own configuration is a decision below.
- **Lifecycle:** one daemon per checkout: started on first use under a lock or pidfile,
  health-checked, restarted on crash, and shut down after an idle period long enough to cover a task.
- **Memory budget per live checkout:** at least 2.2 GB for rust-analyzer and rising with use, plus
  0.3–1.4 GB for the Python server, 0.1–0.3 GB for the target directory, and ty's background CPU.
  With many worktrees that multiplies, and that is the argument for snapshot mode. Test the daemon
  inside Codex's sandbox; a sandbox has broken a language server before
  ([codex#37430](https://github.com/openai/codex/issues/37430)). Fall back to snapshots where a
  daemon cannot run.
- **Snapshot mode:** `rust-analyzer scip` for Rust, about 25–30 s per commit, built once per commit
  hash and shared across worktrees at that commit.
  - It answers `callers`, `refs` and `def` as referencing functions: no call/value labels, `use`
    lines filtered, the trait join heuristic and labelled, and duplicate symbols flagged.
  - Its header gives a distinct precision class, `snapshot <commit>, references`.
  - For Python, a snapshot has two routes, both measured on `py/` only: pyrefly's `--report-glean`
    call edges (read without Glean; 47.5 s and 2.5 GB), or a custom reader over the scip-python
    index. The stock SCIP conversion fails on this repo.
- **Versions:** pin rust-analyzer and ty by version. On any upgrade, re-run the 8 Rust and 5 Python
  targets as a differential canary, with the expected rows checked in, and re-check readiness
  detection and peak memory. rust-analyzer ships weekly and has had memory-leak regressions; ty and
  pyrefly release weekly or faster.

### Phases

0. **Now, no code: native tools, configured properly.**
   - Rust: a small custom Claude Code plugin rather than the official one. Its `.lsp.json` sets:
     - `linkedProjects = ["rs/Cargo.toml"]`;
     - an absolute path to a real rust-analyzer binary (on some hosts the `rust-analyzer` on `PATH`
       is a rustup proxy without the component);
     - `rust-src` or `cargo.sysrootSrc`;
     - its own `cargo.targetDir`, and `checkOnSave` off;
     - `workspace.symbol.search`;
     - `diagnostics: false`.
   - Python: the real `pyright` for the `pyright-lsp` plugin, or a custom `.lsp.json` running
     `ty server` with the search path. Pyright stays stale after shell edits in Claude Code.
   - An optional readiness proxy such as lsp-det holds requests until the server is ready and sends
     the missing file-change events. It is single-maintainer and weeks old.
   - An AGENTS.md line: for trait methods, query the trait declaration; an empty answer means unknown,
     not unused, so retry once and confirm with `rg`.
   - This covers Claude Code only.
1. **Gate: a repo-local A/B.** Run 20–30 "who calls X" tasks, weighted toward common and colliding
   names, three ways: grep only, the phase-0 native tool, and a quick `codenav` prototype. Record
   total task tokens, correctness, and which tool the agent chose unprompted. The 13 measured targets
   (211 Rust and 120 Python call sites) are the acceptance set. Proceed only if `codenav` clearly
   beats the native tool.
2. **`codenav` for Rust** over a live rust-analyzer.
   - `callers`, `refs`, `def`; walk-up; compact rows; capped text rows; readiness and freshness.
   - A test that resolves every `fn new` (89) and `fn run` (40) by qualified path and reports
     ambiguity and failure rates. Name resolution is the riskiest untested piece: every
     name-addressed tool measured here stumbled on it.
   - The repository's tool obligations: `quickstart`, `userguide` and per-subcommand help, a skill,
     the AGENTS.md line, and a `RELATED_WORK.md` seeded from this proposal's comparison.
   - The thin MCP adapter over the same engine, so Codex agents get the same answers.
3. **The Python backend** (ty), `twin`, and `impls`.
4. **Snapshot mode.**

### What not to do

- Do not stand up Glean for this repo.
- Do not use semcode or any tree-sitter graph as the source of truth for callers.
- Do not expose raw LSP JSON to agents.
- Do not make agents pass columns.
- Do not treat an empty answer as "unused": say what was searched and how fresh it was.

## Decisions for the owner

1. **Whether to build at all.** Should phase 1's A/B decide whether to build phase 2, or is the
   native tool (phase 0) enough if it performs well?
2. **Language and pairing.** Many tools here have paired Rust and Python implementations checked by
   differential tests. Should `codenav` follow that convention, or start as a single implementation?
   The `--format tsv` output supports either.
3. **Distribution.** Should the repo ship the phase-0 Claude Code plugin, so that setup is one install
   step?
4. **Shared servers.** Is a per-checkout daemon acceptable for memory given the number of concurrent
   agents and worktrees, or should snapshot mode come first? Measure N concurrent worktrees before
   deciding.
5. **Configuration location.** Should the Python search path be checked in at the repo root, or kept
   in `codenav`'s own configuration and passed at server start? Snapshot indexes and daemon sockets
   should live outside the tree.
6. **Name.** `codenav` is a placeholder.

## Reproducing the measurements

- **Snapshots:** the measurements used `git archive` snapshots of commit `9383f6a`.
- **Toolchain:** Rust runs used a toolchain that has `rust-src`, because the host's default stable
  toolchain did not. Python runs added a snapshot-only ty configuration with `extra-paths` and
  `include` set to `py`, `cross` and `scripts`.
- **Scripts:** the measurement scripts were kept out of the tree on a temporary scratch directory.
  They include a rust-analyzer LSP client with the readiness wait, a SCIP-to-SQLite callers query,
  the compact renderers, and the Python ground truth. Phase 1's prototype should start from them;
  ask for them before they are cleaned up.

## Sources

Linked inline above. The main ones:

- rust-analyzer issues: [#19358](https://github.com/rust-lang/rust-analyzer/issues/19358) (trait
  dispatch), [#14031](https://github.com/rust-lang/rust-analyzer/issues/14031) (SCIP relationships),
  [#18140](https://github.com/rust-lang/rust-analyzer/issues/18140) (SCIP is single-threaded),
  [#4712](https://github.com/rust-lang/rust-analyzer/issues/4712) (no persistent cache)
- SCIP: [scip-code/scip](https://github.com/scip-code/scip), [crux](https://github.com/pedr0v/crux),
  [scipq](https://github.com/elodhorvath/scipq),
  [scip-python#223](https://github.com/sourcegraph/scip-python/issues/223)
- Claude Code: [changelog](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md) (2.0.74,
  2.1.280), [plugins reference](https://code.claude.com/docs/en/plugins-reference#lspservers),
  [tools reference](https://code.claude.com/docs/en/tools-reference),
  [MCP](https://code.claude.com/docs/en/mcp),
  [official plugins](https://github.com/anthropics/claude-plugins-official)
- Bridges: [Serena](https://github.com/oraios/serena),
  [Serena client docs](https://github.com/oraios/serena/blob/main/docs/02-usage/030_clients.md),
  [serena#1556](https://github.com/oraios/serena/issues/1556),
  [mcp-language-server](https://github.com/isaacphi/mcp-language-server),
  [lsp-det](https://github.com/tagawa0525/lsp-det/blob/main/docs/research/claude-code-dogfooding.md),
  [VS Code usages tool](https://github.com/microsoft/vscode/blob/HEAD/src/vs/workbench/contrib/chat/browser/tools/usagesTool.ts)
- Codex: [codex#8745](https://github.com/openai/codex/issues/8745),
  [codex#37430](https://github.com/openai/codex/issues/37430),
  [features](https://github.com/openai/codex/blob/main/codex-rs/features/src/lib.rs)
- Glean: [repository](https://github.com/facebookincubator/Glean),
  [#732](https://github.com/facebookincubator/Glean/issues/732),
  [#688](https://github.com/facebookincubator/Glean/issues/688),
  [#485](https://github.com/facebookincubator/Glean/issues/485)
- semcode: [repository](https://github.com/facebookexperimental/semcode),
  [review-prompts](https://github.com/masoncl/review-prompts)
- Code graphs: [codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp)
- Evidence on benefit and interfaces:
  [lsp-vs-grep study](https://github.com/agentconnect-md/lsp-vs-grep-token-study),
  [Zechner](https://mariozechner.at/posts/2025-08-15-mcp-vs-cli/),
  [Ronacher](https://lucumr.pocoo.org/2025/12/13/skills-vs-mcp/),
  [Willison](https://simonwillison.net/2026/Jul/31/stateless-mcp/),
  [Anthropic on tools](https://www.anthropic.com/engineering/writing-tools-for-agents),
  [advanced tool use](https://www.anthropic.com/engineering/advanced-tool-use)
- Python: [Pylance FAQ](https://github.com/microsoft/pylance-release/blob/main/FAQ.md),
  [pyrefly#3148](https://github.com/facebook/pyrefly/issues/3148),
  [ty changelog](https://github.com/astral-sh/ty/blob/main/CHANGELOG.md),
  [mypy daemon](https://github.com/python/mypy/blob/master/docs/source/mypy_daemon.rst),
  [Jedi references](https://github.com/davidhalter/jedi/blob/master/jedi/inference/references.py)
