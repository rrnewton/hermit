# herdr-run

Run an **allowlisted** command in a terminal pane belonging to a separate terminal server, and get
its real stdout, stderr, and exit code back.

```
herdr-run --agent release-agent run 'git ls-remote origin main'
```

The pane is not a child of the calling process, so whatever constrains that process does not
constrain the command: not its network policy, not its environment, not its lifetime. A pane also
outlives the caller, carries a different ambient environment, and can be watched and taken over by
a person.

One common case, kept as an example rather than as the definition: an AI coding agent inside a
sandbox that blocks the network cannot `git push` or run `gh`, even though it legitimately needs
to in order to publish its work. The terminal server runs outside that confinement, so a shell in
one of its panes is not confined either. `herdr-run net-doctor` checks exactly that situation and
says so before it starts.

> **The allowlist is a cooperative safety rail.** It prevents accidental and cooperative misuse; it
> is not a containment boundary against a hostile process running as the same user. Read the trust
> model in the user guide before widening policy.

{{DISTRIBUTION}}

## What it does

- **Idempotent bring-up.** Finds or creates the Herdr workspace, the per-agent tab, and its pane.
  Running it twice does the setup work once; running it against half-built state completes only the
  missing part.
- **Allowlist by construction.** The command is split into argv, the program name is checked against
  a per-project allowlist, and every token is re-quoted before it reaches a shell - so shell
  metacharacters become literal arguments instead of a second command.
- **Real results, not screen scraping.** The pane redirects into a spool directory, preserving raw
  bytes and a genuine exit code. Raw mode passes those bytes through; JSON mode also includes
  base64 fields alongside readable UTF-8-with-replacement text.
- **Conservative readiness.** The foreground process group must prove the shell is idle twice; a
  prompt-tail check independently vetoes a half-typed human command when it has positive evidence.
- **Serialized account-wide use.** An account-global resolution lock prevents duplicate first-run
  workspaces/tabs, and an account-global per-pane lock spans readiness, launch, and collection, so
  callers from different projects cannot inject into the same pane concurrently.
- **Bounded tab growth.** Every agent that runs a command leaves a tab behind and nothing closes
  it, so the workspace grows until the Herdr server is the bottleneck (measured: 260 panes, >1000%
  CPU, every control call timing out). At the `max_panes` ceiling a NEW tab replaces the
  least-recently-used tab whose shell is provably idle, and is refused only when every tab is busy;
  an existing tab is never refused, a running command never closed, and `herdr-run reap` reports
  which tabs are provably finished with.
- **Visible, best-effort audit.** Refusals, admissions, failures, and completions are appended to a
  private JSONL log. Storage failures warn but never replace a completed command's exit status.

## Configuration

Policy is per project, in the nearest `.herdr-run.yaml`. `herdr-run init` writes an annotated one
into the current directory with every knob present, commented, and set to the value already in
force -- so adopting it changes nothing until you edit something, and that file, rather than any
documentation, is the reference for what is configurable. A file an earlier release wrote keeps
that release's deny lists; herdr-run warns about each built-in entry it leaves out, and
`herdr-run init --upgrade` brings it up to date.

`allow` is a **human-only knob**: an agent that can widen its own allowlist does not have one.
Keep `.herdr-run.yaml` at the top of the project, outside whatever directory the agents write in.
`allow: ["*"]` is a supported allow-everything mode for projects where the pane is not meant to be
a trust boundary at all.

## Quick reference

```
herdr-run status                 what configuration, policy, and session are in effect
herdr-run init                   write an annotated .herdr-run.yaml in this directory
herdr-run init --upgrade         bring the .herdr-run.yaml here up to date with this release
herdr-run run '<command>'        run the command; exits with the command's own exit code
herdr-run check '<command>'      policy only: allowed or refused. Touches no pane.
herdr-run target                 resolve/bring up the pane; print ids and readiness
herdr-run config                 print the effective configuration
herdr-run reap                   report which command tabs are provably finished. Closes nothing.
herdr-run net-doctor             smoke-test one scenario: a caller whose own network is blocked
herdr-run quickstart             the one-screen introduction
herdr-run userguide              the full user guide
```

Requires Linux with a working systemd user manager and a separately installed
[`herdr`](https://github.com/herdrdev/herdr) command in a fixed location such as `/usr/local/bin`,
`~/.local/bin`, or `~/bin`. The integration is tested with Herdr 0.8.0; compatible
newer releases must provide its `status`, `workspace`, `tab`, and `pane` command APIs.

Start with `herdr-run quickstart`, one screen. `herdr-run userguide` is the complete reference.
