# herdr-run — user guide

Run an **allowlisted** command in a terminal pane belonging to a separate terminal server, and get
its real stdout, stderr, and exit code back.

```
herdr-run --agent release-agent run 'git ls-remote origin main'
```

The pane is not a child of the calling process. Whatever constrains that process therefore does
not constrain the command — its network policy, its environment, its working directory, its
lifetime. `herdr-run` makes that one narrow, audited, allowlisted door rather than an ad-hoc pile
of keystroke-injection calls.

> **Read the trust model before widening `allow`.** The allowlist is a cooperative safety rail: it
> keeps a well-behaved caller inside a policy. It is not a containment boundary against a hostile
> process running as the same user, and it makes no claim to be one.

{{DISTRIBUTION}}

### Prerequisites

`herdr-run` requires Linux with a working systemd user manager and a separately installed
[`herdr`](https://github.com/herdrdev/herdr) command. The integration is tested with Herdr 0.8.0;
compatible newer releases must provide its `status`, `workspace`, `tab`, and `pane` command APIs.

---

## Why you would want this

A pane on a separate terminal server differs from a subprocess in ways that are occasionally
exactly what you need:

- **It is not inside your confinement.** A process under a restrictive network policy, a seccomp
  filter, or a container cannot reach something the terminal server can. Running the command in a
  pane sidesteps that without weakening the caller's own policy.
- **It outlives you.** The pane keeps running after the calling process exits, so a long operation
  is not tied to the lifetime of whatever launched it. When a command outruns its timeout,
  `herdr-run` says so and leaves it running rather than killing it.
- **It has a different ambient environment.** A different `PATH`, a different `HOME`, a login
  session, credentials an unattended process does not carry, a working directory somewhere else on
  the machine.
- **A human can watch it and take it over.** The pane is a real terminal somebody can look at,
  scroll back through, and type into. That is why readiness detection is conservative: the tool
  refuses to type over a person rather than assuming the pane is its own.
- **Every use is on the record.** Refusals, admissions, failures, and completions are appended to a
  private log, and each run keeps its command, byte-exact output, and exit code on disk.

What you give up is that the command runs somewhere your process does not control. Hence the
allowlist, the readiness checks, the locking, and the audit log — most of this guide is about
making that trade safely.

### One example: an agent whose sandbox blocks the network

The scenario this was first built for, kept here as an example rather than as the definition. An
AI coding agent runs inside a sandbox whose network policy blocks a destination it legitimately
needs in order to publish its work:

```
$ git ls-remote https://github.com/example-org/example-repo main
fatal: unable to access 'https://github.com/example-org/example-repo/': CONNECT tunnel failed, response 403
```

The terminal server runs outside that confinement, so shells in its panes are not confined, and the
same command through a pane returns the real SHA. `herdr-run net-doctor` checks precisely this
situation and nothing else. The rest of the tool has no opinion about sandboxes: it is a way to run
a policy-admitted command in a pane.

---

## Usage

The shape is `herdr-run [GLOBAL OPTIONS] <subcommand> [OPTIONS]`. There is no subcommand-less form:
running a command is what `run` is for.

```
herdr-run run '<command>'          # run it; exits with the command's own exit code
herdr-run check '<command>'        # policy only: allowed or refused. Touches no pane.
herdr-run status                   # what is in effect here. Changes nothing.
herdr-run init                     # write an annotated .herdr-run.yaml in this directory
herdr-run init --upgrade           # bring the one here up to date with this release
herdr-run target                   # resolve/bring up the pane; print ids and readiness
herdr-run config                   # print the effective configuration as JSON
herdr-run reap                     # report which command tabs are provably finished. Closes nothing.
herdr-run net-doctor               # smoke-test one narrow scenario (see below)
herdr-run userguide                # this document
```

**Global options** identify the invocation and its configuration, and go **before** the subcommand:

| Option | Effect |
| --- | --- |
| `--config PATH` | Use an explicit configuration file instead of the discovered one. Refused by `init`, `quickstart`, and `userguide`, which take no configuration file: `init` works on the one in the current directory, and the other two print a packaged document. |
| `--agent NAME` | The agent this invocation speaks for; names its tab. Otherwise the first of `$HERDR_RUN_AGENT`, `$DG_AGENT_NAME`, and `$ORC_AGENT_NAME` that is non-empty once Unicode white space is trimmed from both ends (so a variable holding only U+3000 or U+00A0 is skipped like an empty one), else `unknown-agent`. An agent name, from the option or a variable, may contain no control character, no invisible character (a format character such as U+200B or U+202E, a default-ignorable code point such as U+034F or U+FE0F, a blank such as U+2800 or U+3164, or a space other than U+0020), and no noncharacter, private-use character, or code point Unicode 16.0 leaves unassigned, as listed for policy names under the configuration file; one that does is a configuration error (78). So is a variable that is not valid UTF-8, rather than being skipped, so an invocation never speaks for the next variable's agent by accident. Refused by `check`, `init`, `reap`, `quickstart`, and `userguide`, which name no tab. |
| `--json` | Emit machine-readable output where the subcommand has one. Refused by `net-doctor`, `quickstart`, and `userguide`, whose only output is prose. `config`, `target`, and `reap` print JSON with or without it. |
| `--version` | Print the version and exit. |
| `--dump-surface` | Print every subcommand and option, with the value each option takes, as JSON, and exit. Meant for tools that check the command-line surface. |

**`run` options** only mean something to `run`, and go **after** it:

| Option | Effect |
| --- | --- |
| `--dry-run` | Admit the command and print the exact rendered line; execute nothing. |
| `--wait-ready S` | Wait up to `S` seconds for the pane to go idle instead of refusing at once. |
| `--timeout S` | Override the command timeout, in seconds. |
| `--cwd PATH` | Working directory for the command. An absolute path is used exactly as written; a relative one is resolved against the project root. |
| `--no-cache` | Ignore the session cache and re-resolve the pane from labels. |

`init` takes `--force` or `--upgrade` (not both), and `target` also takes `--no-cache`. Every other
subcommand takes no options of its own.
Offering an option at the wrong level is an error that says which level it belongs to, so
`herdr-run --cwd /tmp run …` tells you to write `herdr-run run --cwd /tmp …` rather than failing
as an unrecognized argument. Run `herdr-run <subcommand> --help` for one subcommand's own options;
neither level's help documents the other's. An option at the right level that the chosen subcommand
still cannot observe is refused too: `herdr-run --config P init` reads as an instruction to write
`P` and would in fact write `./.herdr-run.yaml`, so it fails and says so rather than doing
something else quietly. All three globals work that way — each is refused by exactly the
subcommands that cannot observe it, with a message saying why, and each of those subcommands says
so in its own `--help`. Being told at the flag is the point: a `--json` accepted by a subcommand
that only prints prose would be discovered at the parse of the output, where it looks like corrupt
data rather than a request that was never going to be honoured.

```
herdr-run --agent release-agent run --timeout 60 'with-proxy git ls-remote origin main'
```

By default stdout and stderr are passed straight through and **the process exits with the command's
own exit code**, so `herdr-run run` composes in a shell script like the command it wraps.

### Exit codes

Wrapped-command statuses are passed through unchanged. Wrapper failures use these stable codes:

| Code | Meaning |
| --- | --- |
| `77` | **REFUSED** by the allowlist. Nothing was executed. |
| `75` | **PANE BUSY** — the pane was not observably idle. Nothing was executed; retrying is meaningful. |
| `69` | Herdr server / workspace / tab / pane could not be established. **Not a retry signal**: it covers both a transient bring-up failure and the `max_panes` refusal, which is reached only when every tab in the workspace is busy and clears only when one of them finishes or is closed. `75` is the only code that promises retrying is meaningful. |
| `76` | The command was launched but did not finish before the timeout. It is **still running**. |
| `78` | The project config is malformed. |

A wrapped command can itself return one of those numbers, so the number alone is ambiguous in raw
mode. With `--json`, a completed wrapped command emits a result object (even when nonzero), while a
wrapper failure emits no result object and writes a `herdr-run:` diagnostic to stderr.

Timeout values, from the command line or the configuration file, must be finite, non-negative,
no greater than 31,536,000 seconds (one year), and a whole number of milliseconds (at most three
decimal places): a finer wait means nothing. That shared upper bound prevents a very large finite
value from overflowing a platform clock into an accidental unbounded wait. On the command line,
`S` is written with ASCII digits only: an optional sign, digits, an optional fraction and exponent
(`10`, `2.5`, `1e3`). Whitespace, underscores, and other scripts' digits are refused.

When stdout cannot be written — a full disk, a reader that went away — `herdr-run` says
`herdr-run: cannot write stdout: <reason>` on stderr and exits `1`, and nothing else: output is
never left in a buffer to be retried at exit. A stderr that cannot be written is ignored. A
standard descriptor that was closed before `herdr-run` started reads as `/dev/null`. An interrupt
(Ctrl-C) ends `herdr-run` by the signal itself, even one that arrives while it is still starting,
so a calling loop sees an interrupted process rather than an exit status. Only an interrupt during
the language runtime's own start-up, before any of `herdr-run`'s code has run, is outside its
reach, and is reported however that runtime reports one. A `SIGCHLD` that the caller left ignored
is put back to its default first, so the exit status of every process `herdr-run` starts is read,
never assumed.

Processes `herdr-run` starts itself (Herdr, `git check-ignore` to check the spool, the direct
`net-doctor` probe) are executed directly: a file the kernel cannot execute, such as a script with
no `#!` line, is reported as `Exec format error (os error 8)`, never run through `/bin/sh`.

Each of them is given an environment built from `herdr-run`'s own by one rule. Every entry is
split at its first `=`: what precedes it is the name and what follows the value, so a value may
hold `=` and a name never does. An entry with no `=`, or with nothing before its first `=`, is
dropped; that includes `=A=1`, which some readers take for a variable named `=A`. Of two entries
with one name the first is kept, where it stood, and nothing is reordered. A Herdr call then has
`PATH` removed and `HOME` replaced, where it stands, by the home the account database reports (or
added at the end when there was none). A signal `herdr-run` was started with ignored is still
ignored in each of them, except `SIGPIPE`, `SIGXFSZ`, and `SIGCHLD`, which start at their default
actions.

Quoted values in diagnostics (`program "curl" is not allowlisted`, `got "maybe"`) are written with
printable ASCII as is, `\n`, `\r`, and `\t` as escapes, and every other character as `\u{hex}`,
so a message never writes a bidirectional override or a line separator to the terminal raw. What
Herdr says when a call fails, and a path, a name, or a command that a message otherwise shows as
it is -- the `program=`, `subcommand=`, and `rendered:` that `check` prints for an admitted
command among them -- is quoted the same way as soon as it holds a line break, a control, or any
other character a name may not contain (listed under the configuration file below), so a
subcommand that begins with U+202E is printed as `subcommand="\u{202e}status"` rather than as a
line the terminal reorders. JSON output, from `config`, `--json`, and the files `herdr-run`
writes, escapes DEL, the C1 controls, and every invisible character as `\uXXXX` too, although
JSON does not require it, so a project path holding U+009B or U+202E cannot reach a terminal
through `herdr-run config`; any JSON reader gets the same strings back.

---

## Configuration — `.herdr-run.yaml`

The tool is generic; the policy is per project.

```
herdr-run init
```

writes an annotated `.herdr-run.yaml` into the current directory, the way `git init` writes into
the current directory. **That file is the reference for what is configurable.** Every key is in
it, commented, set to the value already in force — so adopting it changes nothing until you edit
something, and this guide does not restate the list and then drift from it. `herdr-run init`
refuses to overwrite an existing configuration; `--force` overrides that, and `--upgrade` brings
an existing one up to date instead (see "When the file is older than the tool" below).

The nearest `.herdr-run.yaml` (or `.herdr-run.yml`) found by searching upward from the working
directory wins, `.gitignore`-style. With no configuration file at all the built-in defaults apply,
so a project can adopt the tool with no file at all. `herdr-run status` says which file is in
effect and `herdr-run config` prints every value resolved from it.

Two things in that file are worth naming here, because they are decisions rather than settings:

- **`allow` is a human-only knob.** An agent that can widen its own allowlist does not have one.
  The usual arrangement is agents working inside `worktrees/slotNN/` while `.herdr-run.yaml` stays
  at the top of the project, outside their write scope. The generated file says so in place.
- **`allow: ["*"]` is a supported mode**, not a workaround: it admits any program. Everything else
  still applies — control characters are still refused, the program must still be a bare name
  resolved from the pane's `PATH`, wrapper prefixes must still be declared, and every `deny_*` rule
  still bites. `"*"` must be the only entry, because `["*", "git"]` reads narrower than it is.

Unknown or duplicate keys, merge keys, non-finite/excessive/sub-millisecond timeouts,
fractional/negative retention days, retention beyond 365,000 days, a fractional/negative/oversized
`max_panes`, control characters, characters a name may not contain, and unsupported tab placeholders
are hard errors (exit 78). A typo'd policy key must not silently fall back to defaults. Keys are checked in the order the
generated file lists them, so a file with two mistakes reports the first; type names in messages
use the JSON vocabulary (`null`, `bool`, `number`, `string`, `array`, `object`).

The file is read with herdr-run's own parser for a plain subset of YAML, so it means the same thing
to every reader. Accepted: comments; one document, optionally with `---`, `...`, and a `%YAML 1.1`
or `%YAML 1.2` directive; block mappings and sequences (including `key:` followed by `- item` at
the same indentation); flow collections such as `[a, b]` and `{a: [b]}`, which may span lines;
single-line plain, single-quoted, and double-quoted scalars; the core tags `!!str`, `!!int`,
`!!float`, `!!bool`, `!!null`, `!!seq`, and `!!map`. Plain scalars resolve by herdr-run's own
rule, which is close to the YAML 1.2 core schema but not the same, and this is all of it: empty,
`~`, `null`, `Null`, and `NULL` are null; `true`/`True`/`TRUE` and `false`/`False`/`FALSE` are
booleans; `[-+]?(0|[1-9][0-9]*)` (decimal without leading zeros), `[-+]?0o[0-7]+`,
`[-+]?0x[0-9a-fA-F]+`, and `[-+]?0b[01]+` are integers; a decimal with a `.` or an exponent is a
float (`.inf` and `.nan` are refused); everything else is a string. So `yes`, `on`, `1_000`, and
`007` are strings, where the core schema would read `007` as 7, and `0b101` is 5, which the core
schema has no form for. `!!int`, `!!bool`, and `!!null` accept exactly those spellings, `!!float`
any decimal number (`!!float 007` is 7.0), and `!!str` anything. Refused, each with its own
message and a line and column (`<file>: invalid YAML: <what> at line L column C`): anchors and
aliases, block scalars (`|`, `>`), explicit `?` keys (and, inside a flow collection, any plain
scalar starting with `?`, which YAML 1.1 reads as an explicit key and YAML 1.2 as text), a `:`
followed by text at the start of a later line than its plain flow-mapping key (`{x` then `:y` on
the next line, which readers split differently), plain or quoted scalars that continue onto
another line, any other tag or directive, a second document, keys that are not strings, merge keys,
non-finite numbers, a tab outside a quoted scalar or a comment, a raw U+0085, U+2028, or U+2029
anywhere in the file (YAML 1.1 reads each as a line break and YAML 1.2 does not, so the file would
mean different things to different readers; write `\N`, `\L`, or `\P` in a double-quoted string
to mean one), and collections nested more than 128 levels deep.

Every name the policy compares with what is typed -- `workspace`, `tab_name`, and every entry and
key of `allow`, `prefixes`, `deny_global`, `deny_subcommand`, `deny_anywhere`,
`deny_subcommand_option`, `allow_subcommand`, `value_options`, and `shells` -- may contain no
invisible character. That is, written out in both editions rather than taken from a Unicode
library: every format character (U+200B zero-width space, U+FEFF byte-order mark, U+202E
right-to-left override, and the rest of Unicode category Cf); every default-ignorable code point
(Unicode 16.0's `Default_Ignorable_Code_Point`: the combining grapheme joiner U+034F, the
variation selectors U+FE00..U+FE0F and U+E0100..U+E01EF, the Mongolian free variation selectors,
the Khmer inherent vowels U+17B4 and U+17B5, and the code points reserved for more of the same,
such as U+2065 and the unassigned rest of U+E0000..U+E0FFF); the characters that draw as blank
space, U+2800 braille pattern blank, the Hangul fillers U+115F, U+1160, U+3164, and U+FFA0,
U+16FE4 Khitan small script filler, and U+1D159 musical symbol null notehead; and every space
other than U+0020 (U+00A0, U+2028, U+3000, and the rest). A deny entry that looks like `-c` but
ends in an invisible character would never match the `-c` it appears to deny, so it is a
configuration error (78) instead. So is a name holding a character that draws as whatever the
font in use makes of it, often nothing: a noncharacter (U+FDD0..U+FDEF and the last two code
points of every plane, such as U+FFFE and U+10FFFF), a private-use character (U+E000..U+F8FF and
planes 15 and 16), or a code point Unicode 16.0 leaves unassigned. That last list is written out
too, so a character a later Unicode version assigns is refused until herdr-run's tables are
brought forward. Visible non-ASCII names such as `café` are fine, and so is any of these
characters in a value that is not a name, such as `prompt_tail`. The same rule applies to an
agent name.

How a deny entry matches. A denied option is refused written out, with an attached value
(`--upload-pack=CMD`), and in two more spellings, wherever its rule applies:

- **A long option also matches every abbreviation of it**: `--upload-pac=CMD`, `--upload-p CMD`,
  even `--u`. Git's option parser accepts any prefix of a long option that is unambiguous among the
  options of the subcommand at hand, and which prefixes those are depends on the subcommand and the
  Git version, so herdr-run refuses every prefix rather than guess. An option that merely starts
  the same way is a different option and is not refused for it: `--exec-path` is not `--exec`, and
  `--upload-packs` is not `--upload-pack`. Spell an option out in full if its abbreviation happens
  to be refused.
- **A one-letter short option also matches anywhere in a cluster of short options**, with or
  without an attached value: a denied `-u` refuses `-qu/tmp/x`, and a denied `-Z` refuses `-Zfoo`,
  `-Z=foo`, and `-qZfoo`. This is deliberately fail-closed: an argument after the subcommand that
  starts with `-` and holds the letter is refused too.

Where a rule applies: `deny_global` among the options before the program's subcommand (for
`cargo`, which accepts them on either side, everywhere); `deny_anywhere` in every argument;
`deny_subcommand_option` (program, then subcommand, then options) in every argument after that
one subcommand, for an option that names a program only there -- `git clone -u CMD` is
`--upload-pack`, while `git push -u` is not; and `deny_subcommand` on the subcommand itself, which
is the first argument that is not an option or the value of a `value_options` entry. The
generated `.herdr-run.yaml` lists the defaults, among them `--exec` (the older spelling of
`--upload-pack` and `--receive-pack`, and the program `git archive --remote` runs) anywhere,
`git clone -u`, `git rebase -x`, and `git difftool -x`/`--extcmd`, and every top-level Git option
that takes the next argument as its value (`--attr-source`, `--shallow-file`, and
`--super-prefix` among them), so that `git --attr-source HEAD daemon` is read as `git daemon`.

`value_options` is the one list that adds to its built-in table rather than replacing it. Which
options take the next argument is each program's own syntax, not a policy choice, and leaving one
out could only make the policy misread a command, so a configuration can add an option, for any
program, but cannot take a built-in one away; `herdr-run config` prints the table in force.

The directory a command runs from, `HOME` when a command uses `~`, and the agent variables must be
valid UTF-8; otherwise herdr-run stops with a configuration error rather than converting them
lossily and writing its spool, audit log, or session cache somewhere else.

### When the file is older than the tool

Every list set in `.herdr-run.yaml` except `value_options` replaces the built-in one, and `init`
writes every list out in full. So a file keeps the defaults of the release that wrote it: an entry a later release
adds to a built-in deny list does not reach a file written earlier. A file written by
`herdr-run init` in 0.2.0 or 0.3.0, or copied from the example the 0.1.0 and 0.2.0 user guides
printed, leaves `--exec` out of `deny_anywhere`, for instance, and so admits `git push --exec=CMD`.

herdr-run says so rather than letting it pass. Every subcommand that reads the configuration prints
one line on stderr naming the file and every built-in deny entry its lists leave out:

```text
herdr-run: WARNING: /project/.herdr-run.yaml leaves out built-in deny entries, so they are not enforced: deny_anywhere "--exec". To adopt them, run 'herdr-run init --upgrade' in /project or add them by hand.
```

The line is the same every time for the same file, so it can be recognised. Entries for a program
the allowlist can never admit are not counted, and neither are Cargo's `--config` and `-Z`, which
are refused whatever the file says. `herdr-run status` lists the same entries on its `deny lists`
line, and as `missing_defaults` with `--json`. A file that leaves an entry out on purpose keeps
drawing the warning: a built-in deny entry can be removed, but not silently. A file that `init`
cannot work on, one named by `--config` with another name, is told to add the entries by hand.

To bring the file up to date, run this in the directory that holds it:

```
herdr-run init --upgrade
```

It rewrites each setting the file still holds exactly as an earlier release wrote it, line for line,
into what this release writes, and leaves every other byte alone: comments, settings you changed,
and lists you reformatted are untouched. It prints each setting it rewrote and the entries that
added, then any built-in deny entry still left out, in a list it did not recognise, for you to add
by hand. Before writing, it loads the rewritten file and checks that it reads exactly as the old
one did, with entries added and nothing else changed; otherwise it writes nothing and says so. The
file is written in place, so its mode, its owner, and a symbolic link to it are kept, and it is not
written at all when there is nothing to rewrite. `--json` gives the same report as one object. A
file that does not load is refused, as is a directory with no `.herdr-run.yaml` or `.herdr-run.yml`.

`value_options` is right in an older file without an upgrade, because it adds to the built-in table
(above). `init --upgrade` rewrites the older, shorter git list anyway, so the file reads as what is
in force.

---

## What is in effect here — `herdr-run status`

```
herdr-run status
```

Answers "what would happen if I ran something here?" the way `git status` does, and answers it by
looking:

```
herdr-run status

  configuration
    file          /project/.herdr-run.yaml
    project root  /project
    spool dir     .herdr-run

  policy
    allow         git, gh
    prefixes      with-proxy
    deny lists    every built-in entry is in force

  session
    agent         release-agent
    workspace     agent-cmds
    tab label     release-agent (not created yet)
    herdr         installed; server is running
    panes         3 in workspace "agent-cmds"

Nothing was changed: status only reads.
```

**Strictly non-mutating.** It starts no server and creates no workspace, tab, or pane — a status
command that brought those up in order to describe them would be reporting on its own side
effects, and the next reader could not tell the difference. Anything it could not observe is
reported as unknown, with the reason, rather than guessed at. `--json` gives the same facts as one
object.

`deny lists` names every built-in deny entry the configuration's lists leave out, the same entries
the warning names (see "When the file is older than the tool"), or says that every built-in entry
is in force.

---

## How it works

### Idempotent bring-up

Server → workspace → tab → pane, each resolved by looking for it before creating it:

1. **Server** — if `herdr status` says no server is running, start one. **Always** through
   `systemd-run --user`.
2. **Workspace** — find by label (default `agent-cmds`), else create it. A new workspace arrives with
   one default tab, which is *renamed* into the tab schema rather than having a second tab added.
3. **Tab** — find by label (default: the agent name), else create it.
4. **Pane** — the tab's single pane. A split tab is refused as ambiguous rather than guessed at.

Resolved ids are cached in `<spool_dir>/session-cache.json`, and the cache is **always re-validated**
against the live session (unique workspace label and id; unique tab label and id; pane, tab, and
workspace relationship) before use. Any mismatch discards it and re-resolves from labels. An
account-global resolution lock covers lookup, creation, and cache publication, so simultaneous
first runs from different projects cannot create duplicate labels. Running the tool twice does the
bring-up work once; running it against half-built state completes only the missing part.

### Why the server must start via `systemd-run`

Panes are children of the Herdr **server**. If a confined process starts the server, every pane it
creates inherits that confinement, and the tool quietly reproduces the very limit it was reached
for — nothing about such a pane looks different from a good one. `systemd-run --user` reparents the
server onto the user manager, clear of the caller's confinement.

The launcher ignores caller-provided `HOME` and `PATH` for brokered execution. It reads the account
home from the user database, resolves `herdr` from a fixed set of install locations, canonicalizes
the result, and passes that absolute path to systemd. This blocks a planted workspace executable;
it cannot make an owner-writable per-user install trustworthy against another same-user process.

If a bad server is already running, `herdr-run net-doctor` will catch it. Its fix is
`herdr server stop`, then let `herdr-run` restart it.

### Readiness detection

Typing into a terminal someone else may be using is the dangerous part. The foreground-process
signal is authoritative and must prove idleness twice. The independent prompt signal can veto on
positive evidence of a half-typed line; when no prompt tail can be inferred it records `abstain`
instead of pretending a check succeeded.

**Primary — foreground process group (an observable).** `herdr pane process-info` reports
`shell_pid` and `foreground_process_group_id`. A shell at its prompt owns its own foreground group;
when anything runs, the kernel moves that group to the job. This dereferences the running process
rather than inferring from pixels, so prompt themes, colour, and shell upgrades do not affect it.
Required for **two consecutive** polls, because a just-submitted line has a brief window before the
pgid moves.

**Secondary — prompt tail (a veto only).** The process signal cannot see a command a human *typed*
but has not run: the shell is genuinely idle, yet typing would concatenate onto their line. So the
last rendered line is checked against the prompt's trailing literal, inferred from `PS1` in the shell
rc file (a `PS1` ending `...\n\$ ` yields `"$ "`):

- line ends with the tail, and it appears once → `clean`
- tail appears earlier in the line, or more than once → `dirty` → **refuse**
- tail not found, or not inferable → `abstain`

The last line is the last non-blank one, split on line feeds only (a trailing carriage return is
dropped), so a Unicode line separator in a half-typed command cannot hide it. The `PS1` is read
with CRLF and CR line ends treated as LF.

`abstain` does not veto an otherwise idle pane. Set `prompt_tail` explicitly for an exotic prompt,
or `readiness: process` to disable this secondary check.

Both verdicts are recorded in each run's `meta.json`, so a run never implies a check that did not run.

### Result capture — files, not screen scraping

The pane invokes a POSIX shell with this inner wrapper, so configured interactive shells such as
Fish do not have to parse POSIX grouping syntax themselves:

```sh
{ cd <cwd> && <command> ; } >stdout 2>stderr; printf '%s\n' "$?" >exit_code
```

into a newly and exclusively created `<spool_dir>/runs/<run_id>/`, and the caller reads those files
directly. Scraping the terminal would inherit hard-wrapping mid-token,
silent scrollback truncation, ANSI/progress-bar corruption, and — decisively — **no exit code
anywhere on screen**. The appearance of `exit_code` is both the completion signal and the result; a
value that is not a complete marker is treated as "still being written", not as corrupt. A
marker is at most 128 ASCII bytes: optional surrounding ASCII whitespace around an optional sign
and decimal digits, within the signed 32-bit range. The same rule decides completion for a run,
for `reap`, and for retention, so nothing is deleted on a marker anything else would not accept.

Raw mode writes `stdout` and `stderr` back without decoding. JSON mode includes `stdout_base64` and
`stderr_base64` for byte-exact consumers and readable `stdout`/`stderr` decoded as UTF-8 with invalid
sequences replaced.

Each run directory holds `command`, `stdout`, `stderr`, `exit_code`, and `meta.json`. Directories the
tool creates for spool, run, cache, and account-global lock state use mode `0700`; command output,
metadata, cache, and lock files use `0600`, independent of the caller's umask. A pre-existing
configured spool directory is never chmoded: only tool-owned children and files are created below
it, so `spool_dir: .` cannot silently change a project directory's permissions.

If writing `meta.json` fails after a command completes, the failure is warned and recorded when
possible, `--json` reports `"meta": null`, and the command's output and exit status are still
returned.

### Retention

Run spools are pruned when a **new run is written**, not by a timer: a scheduled job that silently
stops is indistinguishable from one that runs and finds nothing to do. A run is eligible only after
it has a regular, parseable `exit_code` completion marker, and its age is measured from that file's
mtime. Completed runs older than `retention_days` (four by default) are removed; active, timed-out,
or incomplete runs are retained even if their directory itself is old.

Scope is enforced rather than trusted: a symlink in the configured root or any ancestor disables
that prune pass; entry symlinks are skipped; only directories whose lexical and canonical parent is
the exact runs root are considered; and the root itself is never removed. The **audit log is never
pruned**: it is a separate best-effort evidence trail, and a record that deletes itself would be
misleading.

### The pane cap and `reap`

Every agent that ever runs a command leaves a tab behind, and **no run closes it**. Agents are
coined and destroyed continuously, so the command workspace grows for as long as agents are coined
— until the Herdr server itself becomes the bottleneck. That end state is measured, not feared: on
a 316-core host a session with 260 panes drove the server to over 1000% CPU, with roughly 98% of
cycles in the allocator's futex spinlock and every control call timing out.

`max_panes` (32 by default) bounds the workspace. When it already holds that many panes and this
agent has **no tab yet**, herdr-run makes room by closing the **least-recently-used idle tab** and
then opens the new one. The check is deliberately only on the create path: an agent whose tab
already exists is never locked out of it, because a cap that can break work in progress is a cap
that gets switched off. Set `max_panes: 0` to disable it.

Least recently used means the tab whose most recent run record (the timestamp-prefixed spool
directory) is oldest; a tab with **no** surviving record — never driven by herdr-run, or older than
`retention_days` — counts as the oldest of all. Only tabs in the configured `workspace` are
considered. A tab is closed only when, checked under its pane lock immediately before the close:

1. no other herdr-run holds the pane's lock (a command is being typed or awaited there);
2. the shell alone owns the terminal's foreground process group — the same test a run uses to
   decide the pane is at a prompt;
3. the shell leads its own session and **no other process is in that session**, so a background
   job (`cmd &`) or a stopped job keeps the tab open; and
4. the tab holds exactly one pane, because closing a tab closes every pane in it.

If `/proc` cannot be read, the tab counts as busy. Nothing beyond `herdr tab close` is done: the
run spool and the audit log are untouched. Each replacement is reported on stderr and appended to
the audit log as an `EVICTED` entry carrying the newcomer as `agent`, plus `pane_id`, `tab_id`,
`workspace_id`, `evicted_agent`, `last_run` and `last_run_at`:

```text
herdr-run: replaced idle tab 7 (pane 7-1, agent builder-3, last run 2026-10-01T09:00:00Z) to make room for 'tester-9'
```

Only when **no** tab qualifies does bring-up fail with exit 69. The message names the remedy and
says, for the first three tabs it considered, why each stayed open (`pane 4-1: foreground pgid 812
!= shell pid 790; running: cargo`). One window remains: an agent whose tab is replaced after it resolved
the tab but before it took the pane lock finds its pane gone and fails its readiness check before
anything is typed; retrying brings up a fresh tab.

The cap is **per workspace**, while the measurement behind the number is **per server**. Nine
projects each with their own `workspace:` and each sitting at 31 panes reproduce the measured
condition without any of them breaching its cap. `max_panes` bounds one project's contribution to
the leak, not a host's total.

`herdr-run reap` says which tabs can go. It considers only panes **this tool has run a command in**
(read from the run spool, not from `herdr pane list`, so a human's tab in the same workspace is not
a candidate), and it re-derives scope from the current configuration: the recorded workspace label
must equal `workspace`, and the recorded tab label must equal what `tab_name` renders today for the
recorded agent. Retarget either and the old tabs immediately fall out of scope.

**The candidate set is bounded by retention, and this matters.** Run records are the candidates,
and herdr-run deletes a run record `retention_days` (four by default) after the run finished. A tab
whose owning agent last ran a week ago therefore has no surviving record, is not a candidate, and
will never appear in this report — while still holding a pane and still counting against
`max_panes`. The oldest leaks are the ones `reap` cannot see: at the cap they are the first to be
replaced if their shell is idle, and otherwise they have to be closed by hand.
The report prints `candidate_source.retention_days` for exactly this reason: `"considered": 3` means
three panes were *eligible to look at*, not that the workspace holds three tabs.

The report's `occupancy` block measures that gap instead of only naming it. `live_panes` is what
herdr lists in the workspace — the number `max_panes` is compared against. `with_record` counts the
live panes a surviving run record names, which are the only ones `reap` can judge, and
`without_record` is the rest: tabs holding a share of the cap that no verdict will ever cover. When
`without_record` is most of `max_panes`, closing those tabs by hand is the remedy, and `reap` cannot
help. If herdr cannot list the workspace, the counts are `null` and `listing_error` says why;
a failed listing is never reported as an empty workspace.

A full workspace is not by itself a refusal: at the cap, the next agent's tab replaces the
least-recently-used unsplit tab whose shell is idle. `replaceable` counts the tabs that would
qualify if an agent arrived now, judged by every check the cap uses except the pane lock (a tab
whose lock another herdr-run holds is counted, though the cap would skip it), and `cap_refusal` is the
refusal that agent would be shown when none does (`null` otherwise). This reading takes no pane
lock and closes nothing, so it can change before the next agent arrives; the cap judges again,
under the lock, before it closes anything. `replaceable` is `null` when the listing failed, and
when the survey itself fails `survey_error` says why. A monitor should warn when `live_panes` is
at `max_panes` and `replaceable` is short of what the next tab needs, not merely when the
workspace is nearly full.

A tab is reported STALE only when all three hold, each of them positive evidence:

1. **No in-flight work** — every run naming the pane recorded an `exit_code`. A run without one is
   the agent-is-thinking case, and it wins over every other signal. That state is written when a
   command outlives its timeout: it is still running in a pane nobody owns, so the run is recorded
   with a null exit code rather than not recorded at all. If the command later finishes, its
   `exit_code` file is re-read on the next sweep, so one timeout cannot make a pane permanently
   unreapable.
2. **The pane's shell is gone** — herdr still lists the pane, but `/proc` says the shell pid it
   reports does not exist. Note this is the PANE shell, not the trailing PID in the run directory
   name: that one is the short-lived `herdr-run` CLI and is dead for every completed run, so a
   policy anchored on it would classify every healthy idle agent as stale.
3. **Reuse and reboot are excluded** — identity is the triple `(pid, boot_id, start_ticks)` from
   field 22 of `/proc/<pid>/stat`, recorded in `meta.json` at run time and compared against the live
   process. A recycled pid, a new pane incarnation, or a record from a previous boot is UNKNOWN.

Everything else — an unreadable or unparseable `/proc` entry, a pane herdr no longer lists, a
workspace label herdr cannot resolve, a control call that fails — is UNKNOWN, and UNKNOWN is never
reaped. Only a `/proc` entry that is *positively absent* may contribute to STALE; "could not tell"
never does. **`reap` closes nothing**; it prints the plan, with a reason
for every pane it declined and a count for every verdict including the zeros, because "reaped 0
because nothing was stale" and "reaped 0 because the detector is inert" are otherwise the same
output.

### Audit log

`herdr-run` attempts to append JSON lines to `<spool_dir>/audit.jsonl` for **refusals**, dry runs,
admission before target resolution/launch, wrapper failures, and completed commands. The `net-doctor`
pane probe follows the same path. This makes a successful run a two-phase record (`ADMITTED`, then
`RAN`) and leaves an admission marker when a later control operation fails.

The audit is deliberately best effort: an append failure produces a warning but cannot mask a
completed command's output or exit status. The file is private (`0600`) and each line is issued as
one append write, but it is not fsynced, remotely replicated, immutable, or protected from another
same-UID process. It is operational evidence, not durable or tamper-proof security logging.

### Brokering

Herdr control calls default to `direct`, which works when the server's Unix socket is reachable from
the caller. Set `broker: systemd-run` on a host where it is not. This setting does **not** affect
server startup, which always uses `systemd-run` (above).

---

## Trust and safety model — read before widening `allow`

**What the wrapper guarantees for cooperative callers:** the command is split with POSIX shell-word
rules, the program name is checked against policy, terminal control characters are rejected, and
every token is safely re-quoted before it is embedded in the shell wrapper. So the pane executes
the admitted argument vector. There is no
metacharacter blocklist, deliberately: a list of `;`, `&&`, backticks, `$()`, newlines… is a *proxy*
for "cannot start a second command", whereas re-quoting is the property itself.

Also enforced: the program must be a **bare name** resolved from the pane's `PATH` (so `./git`,
`/tmp/gh`, or `~user/gh`, which `~` expansion would turn into a path, cannot masquerade), and each
wrapper prefix may appear at most once.

A leading `~` in an argument after the program is expanded **before** the policy judges it, and the
policy judges the expanded argument, which is exactly what runs. So `HOME` cannot carry anything
past a rule: with `HOME=--exec-path=/tmp/evil`, `git ~ foo` is judged as
`git --exec-path=/tmp/evil foo` and refused, and with `HOME=daemon` it is judged as `git daemon foo`
and refused. The wrapper prefixes and the program are never expanded.

**`cargo` is a special case and is not allowed by default.** Even `fetch` and `metadata` can execute
ambiently configured compiler wrappers, credential providers, or fetch helpers, so calling those
commands "download only" would overstate the boundary. A project may explicitly add `cargo` to
`allow` when it accepts that trust widening. The built-in `allow_subcommand` policy then limits the
opt-in to dependency-oriented commands; `build`, `test`, `run`, `install`, `clippy` and every
third-party `cargo-*` subcommand remain refused. `--config` and unstable `-Z` flags are refused in
every argument position, including attached forms. Fetching through the pane and then building
locally with `--offline` can still be operationally useful, but it is not a no-code-execution
guarantee.

**What is NOT guaranteed:** anything an allowlisted program can do by itself. `git` writes files,
runs hooks from the repository being operated on, and honours ambient configuration. `gh`
authenticates as you and can modify repositories. The `deny_*` lists remove the best-known
self-escapes (`git -c alias.x='!sh'`, `git --exec-path=…`, `gh extension exec`) but they are
**defense in depth, not the boundary** — treat "the caller can run `git`" as the actual privilege
being granted, and size the allowlist accordingly.

This process runs under the same user identity as the caller. If that caller can access Herdr's
socket or the user-systemd bus directly, it can bypass this command, its project policy, and its
audit. An enforceable security boundary requires a broker outside the caller's confinement that
owns immutable policy and audit storage, plus rules denying the caller direct Herdr and systemd
access. This package does not claim to provide that stronger architecture.

**Operational notes:**

- On timeout the command is **not** killed. It runs in a terminal this process does not own, and
  signalling it blind could hit whatever the pane is doing by then. The error names the pane and the
  spool directory so it can be inspected.
- `spool_dir` holds real command output. It must be git-ignored.
- The panes are shared with a human. Bring-up always uses `--no-focus`, and readiness refuses rather
  than typing over someone.
- An account-global per-pane file lock serializes concurrent callers across projects and processes.
  It cannot eliminate the inherent race with a human typing immediately after the final readiness
  sample.
- Each Herdr/systemd control subprocess is bounded to 30 seconds and killed/reaped on expiry. That
  control bound is separate from the configured timeout of the command running in the pane.

---

## Verifying one narrow scenario

```
herdr-run net-doctor
```

`net-doctor` is deliberately about **one** situation and says so before it does anything: a caller
whose own network access is blocked, reaching the network through a pane that is not blocked. It is
not a health check of the tool as a whole, and a project using `herdr-run` for one of the other
reasons above should not read its verdict as one.

Within that scenario it brackets the premise in **both** directions rather than asserting it: the
probe (`git ls-remote` against `probe_remote`) must **fail** run directly and **succeed** through
the pane. Three verdicts:

- blocked directly, succeeds via pane → this scenario works;
- succeeds both ways → for this scenario the pane is buying nothing, run the command directly;
- fails via pane → the pane path is broken, most likely a server started from inside a confinement.
