# herdr-run — related work

This comparison concerns **shell command execution**: submit a command to a
human-visible terminal, enforce the caller's command policy, and return durable
stdout, stderr, and exit status. For managing coding-agent conversations, native
goals, and chat connections, see the [interactive-agent guide](AGENT_USER_GUIDE.md)
and [Chat guide](CHAT_USER_GUIDE.md).

Provenance: this revision, dated 2026-09-17, replaces the earlier
environment-specific anecdotes with a comparison of public open-source tools.
It preserves the general design lessons, not the original text or measurements.

## Existing tools

| Project | Relevant capability | Relationship to herdr-run |
| --- | --- | --- |
| [Herdr](https://github.com/herdrdev/herdr) | A persistent terminal server with CLI and socket APIs for panes, process information, and interactive agents. | The current terminal backend. Use its native `pane run` command to submit shell commands and `pane process-info` to inspect readiness. Its `agent prompt` API serves interactive agent composers; that is a separate operation from executing a shell command. |
| [tmux](https://github.com/tmux/tmux/blob/master/tmux.1) | Persistent terminals, control mode, input delivery, and pane capture. Current upstream also documents `new-pane -W` returning a command's exit status and status supplied through OSC 133 shell integration. | A viable terminal substrate with useful execution primitives. These capabilities do not imply a tmux backend exists in herdr-run; its current client is specific to Herdr. Captured terminal output still differs from separately recorded stdout and stderr. |
| [OpenSSH](https://github.com/openssh/openssh-portable/blob/master/ssh.1) | Executes a remote command, supports sessions without a PTY, and returns the remote exit status; transport errors return 255. | Prefer a direct command protocol when it fits the workflow. It does not require interpreting a screen. Reusing a visible local shell and preserving per-invocation results across caller restarts are different requirements. |
| [Pexpect](https://github.com/pexpect/pexpect/blob/master/doc/api/pexpect.rst) | Starts and drives interactive child processes, sends input, and matches expected output. | Useful when interaction with a PTY is required. Prompt matching needs application-specific handling; a match is not a general shell-readiness or command-completion contract. Pexpect can observe its child process's exit, which is distinct from each command run inside a persistent shell. |

These are complementary building blocks. This comparison does not claim that
file capture, allowlists, or terminal automation are new inventions, or that the
listed tools exhaust the available implementations.

## Lessons used in herdr-run

**Use the terminal's command API.** Herdr already provides pane creation,
command submission, process inspection, and terminal reads. The shell runner
uses those operations rather than assembling raw text and synthetic key events.
Interactive agent submission belongs to Herdr's agent API and the separate
agent-control layer.

**Keep authoritative results outside the terminal rendering.** Terminal output
can wrap, overwrite earlier rows, contain control sequences, and outgrow
scrollback. A rendered screen also does not preserve separate stdout and stderr.
The command wrapper therefore writes each stream to its own file and records
the command's actual exit status. The caller reads bytes from those files;
readable output alone never establishes success. Durable completion records
also distinguish a finished command from a partial spool left by interruption.

**Treat readiness as a separate decision.** A persistent shell may have a
foreground job or an unfinished command in its input buffer. herdr-run uses
foreground process information as its primary readiness signal. Its terminal
read is limited to detecting evidence of an unfinished input line: an
unrecognized prompt cannot establish that the composer is empty. Native
introspection reduces dependence on prompt themes and terminal redraws, but
does not make every interactive shell state observable.

**Keep execution policy and results explicit.** herdr-run adds command
admission, literal argument quoting, durable invocation records, and an audit
trail including refusals around the selected terminal backend. Those contracts
are the reason for the wrapper; terminal launch and transport remain delegated
to Herdr.

Sources above were checked against the public upstream repositories on
2026-09-17. Upstream capabilities and installed versions can differ; consult the
installed command help before relying on a particular operation.
