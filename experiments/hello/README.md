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
$ export DYNAMORIO_HOME=$HOME/dynamorio/install          # marks the backend available
$ export HERMIT_DRRUN=$HOME/dynamorio/install/bin64/drrun
$ export HERMIT_DBI_CLIENT=<reverie>/target/debug/reverie-dbi-native/libreverie_dbi_client.so
$ hermit run --backend dbi -- /bin/echo hello
hello
```

`reverie-dbi` is an in-process DynamoRIO client (built outside Cargo because
DynamoRIO's CMake package supplies the client linker flags). `hermit run
--backend dbi` shells out to `drrun` with that client, which rewrites the guest
in-process, counts branches, replaces `CPUID` with the deterministic identity,
and forwards `write` through a Reverie `Tool`. `DYNAMORIO_HOME` (or
`DynamoRIO_DIR`) marks the backend available; without `HERMIT_DRRUN` /
`HERMIT_DBI_CLIENT` it prints an actionable error.

Unlike the KVM prototype, the DBI backend loads and runs the *real* guest ELF
(`/bin/echo` above executes under DynamoRIO). It does not yet drive Detcore's
scheduler, so it is not a full determinism backend, but the interception path
(branch counting, syscall capture, deterministic CPUID) is real.

> **Client revision caveat.** The client built from the `reverie` revision that
> `hermit-cli` currently pins (`e3e2c965`) **stack-overflows/SIGSEGVs** on
> dynamic binaries such as `/bin/echo` and `/bin/true`. Build the client from a
> `reverie` revision on the DBI development line that fixes this — verified with
> `69f47d9` ("DBI parity: virtualize clocks and resource limits"), which runs
> `/bin/echo hello` cleanly. `hermit-cli` does not link `reverie-dbi` in Rust
> (it only shells out to `drrun`), so the client revision is chosen at client
> build time and is independent of the pinned `reverie` used by the ptrace/kvm
> backends.

### Build recipe (required — use a source build, not a prebuilt release)

A **prebuilt** DynamoRIO release (e.g. 10.0.0) does **not** work: its private
loader cannot satisfy the Rust std runtime's symbol/TLS needs and `drrun` fails
with `<ERROR: using undefined symbol!>`. A **source build of DynamoRIO main**
(verified at 11.91) fixes this. Recipe:

```bash
# 1. Build + INSTALL DynamoRIO from source (the install tree is required: the
#    build-tree CMake package omits the Release-config imported locations for
#    the drx/drmgr/drreg extensions the client links against).
with-proxy git clone --recursive --depth 1 https://github.com/DynamoRIO/dynamorio.git ~/dynamorio
cmake -S ~/dynamorio -B ~/dynamorio/build \
  -DCMAKE_BUILD_TYPE=Release -DBUILD_TESTS=OFF -DBUILD_SAMPLES=ON
cmake --build ~/dynamorio/build --parallel
cmake --install ~/dynamorio/build --prefix ~/dynamorio/install

# 2. Build the reverie-dbi native client against that install tree.
cd <reverie checkout>
DYNAMORIO_HOME=~/dynamorio PROFILE=debug bash reverie-dbi/scripts/build-client.sh
# -> <reverie>/target/debug/reverie-dbi-native/libreverie_dbi_client.so

# 3. Point hermit at them (see the exports above) and run --backend dbi.
```

Validation with the source build: `reverie-dbi/scripts/test-echo.sh` and
`test-cpuid.sh` both pass (the latter prints
`CPUID-SUCCESS vendor=GenuineIntel signature=00000663`, confirming the
deterministic CPU identity with RDRAND/RDSEED/TSX/AVX-512 masked), and the client
reports non-zero `branches`/`syscalls`/`rewritten_writes`.
