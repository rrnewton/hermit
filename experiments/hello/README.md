# Hello world on all three backends

`hermit run` now takes `--backend {ptrace,dbi,kvm}` to select the execution
mechanism. All three share Detcore/Reverie's `Tool`/`Guest` contracts; they
differ in how the guest is executed and intercepted.

```bash
gcc -O2 -o experiments/hello/hello experiments/hello/hello.c   # dynamic hello
```

## ptrace (production) — runs the real ELF

```bash
$ hermit run --backend ptrace -- ./experiments/hello/hello
hello world
```

seccomp + ptrace, out-of-process tracer. Runs arbitrary ELF guests. This is the
default, so `hermit run -- ./experiments/hello/hello` is equivalent.

## kvm (experimental) — hello world via a VM

```bash
$ hermit run --backend kvm -- ./experiments/hello/hello
hello world
```

`reverie-kvm` is not yet a Linux ELF loader (one real-mode vCPU, no process
lifecycle), so it does not exec the ELF. Instead it runs a built-in guest that
issues `write(1, "hello world\n")` through a `vmcall`; the VM-exit is decoded to
a Reverie syscall and the host handler performs the write, so the bytes reach
the real stdout. The vCPU gets the deterministic CPUID policy (RDRAND/RDSEED/
TSX/AVX-512 masked). Requires `/dev/kvm` (usable by non-root here).

## dbi (experimental) — DynamoRIO instrumentation

```bash
# Requires the DynamoRIO SDK + the reverie-dbi native client:
export HERMIT_DRRUN=<dynamorio>/bin64/drrun
export HERMIT_DBI_CLIENT=<...>/libreverie_dbi_client.so   # from reverie-dbi/scripts/build-client.sh
hermit run --backend dbi -- ./experiments/hello/hello
```

`reverie-dbi` is an in-process DynamoRIO client (built outside Cargo because
DynamoRIO's CMake package supplies the client linker flags). `hermit run
--backend dbi` shells out to `drrun` with that client. Without the two env vars
it prints an actionable error.

Status: the DBI toolchain builds end to end (Rust runtime cdylib + native
client link, and `drrun` launches the client), but running the Rust runtime
inside DynamoRIO's **private loader** currently fails with
`<ERROR: using undefined symbol!>` when using a prebuilt DynamoRIO release —
DynamoRIO's private loader cannot satisfy the Rust std runtime's symbol/TLS
needs. Fully running DBI needs a source-built DynamoRIO matching the reverie
team's configuration (and/or a slimmer runtime). The `--backend dbi` wiring is
in place for when that lands.
