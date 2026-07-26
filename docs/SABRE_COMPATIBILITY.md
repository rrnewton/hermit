# SaBRe Compatibility Status

Status measured 2026-07-25 on Linux x86-64.

## Classification

The CLI spelling `--backend=sabre` currently selects a host-direct compatibility
adapter. It launches the external `reverie-sabre-strace` runner, pinned SaBRe
loader, and `libreverie_sabre_strace_plugin.so`. The plugin drives Reverie's
shared `StraceTool` through `ReverieAdapter`.

This path does not instantiate `Detcore<SaBReGuest>`, implement a Reverie
`Backend`, link `reverie-sabre` into `hermit-cli`, or run under ptrace.
Its backend-reality score is therefore B0 even when programs exit successfully.

## Measured envelope

The measurement used:

- Hermit base: `29c53e3925f66c948bd08764e8a1289a83f42555`.
- Reverie: `124ead67804706d663ce9f8d2df02d7d2cb3fb15`.
- SaBRe loader: `34065e7ddae6f1c90db7e0bf5c22a9aa89f9d605`.
- Log level: default.
- Relaxations: the adapter runs directly on the host without Hermit's ptrace
  Detcore runtime or namespace isolation.

`./validate.sh --sabre-compat-only` selected 159 installed programs and passed
all 159 in 48 seconds. The overall 183-row report showed 159 passes and 24
unavailable or unmeasured rows. Representative passes include `true`, `echo`,
`cat`, `bash`, `git`, `java`, `node`, `python3`, `sqlite3`, and the
compiler and binutils tools.

Each `--strict --verify` compatibility row runs the program twice and compares
exit status, stdout, and stderr. It does not compare Detcore event logs and does
not establish L1 or L2 determinism.

Run the focused gate with explicit artifacts:

```bash
HERMIT_SABRE_RUNNER=/path/to/reverie-sabre-strace \
HERMIT_SABRE_BINARY=/path/to/sabre \
HERMIT_SABRE_PLUGIN=/path/to/libreverie_sabre_strace_plugin.so \
./validate.sh --sabre-compat-only
```

## Known limits

- Only dynamically linked Linux x86-64 programs with loader-supported mappings
  are in the measured envelope. Static, JIT, and non-x86-64 programs are not.
- A Meta `git.meta.real` binary using the custom
  `/usr/local/fbcode/platform010/lib/ld.so` interpreter segfaults under SaBRe;
  the distro `/usr/bin/git` using `/lib64/ld-linux-x86-64.so.2` passes.
- The process name remains `sabre` after loading a guest. Name-based
  `pgrep -x <guest>` is not a valid compatibility assertion; parent/child PID
  relationships work.
- Host PID 1 is not owned by the guest user. Signal-zero probes must target a
  process they own rather than assuming PID-namespace root privileges.
- No Detcore syscall models, scheduler, virtual time, deterministic randomness,
  CPUID virtualization, record/replay, PMU preemption, or namespace isolation
  are active.
- Rewriting is not fail closed for every executable mapping. Signals and native
  thread scheduling do not have ptrace-equivalent control.

## Hybrid direction

Simply launching SaBRe under ptrace is not a correct hybrid. Rewritten guest
syscalls and the plugin's own RPC/control syscalls execute in one process; without
event provenance, ptrace can misclassify controller operations as guest events.

A credible SaBRe-plus-ptrace design must keep ptrace/Detcore as the sole semantic
authority, distinguish rewritten guest operations from control traffic, preserve
ptrace lifecycle and namespace setup, and fail closed on incomplete rewrite
coverage. Until that protocol exists and passes the ptrace strict-verify corpus,
SaBRe results remain compatibility evidence only.
