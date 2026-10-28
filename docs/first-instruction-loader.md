# First-instruction loader integration

`hermit run --in-guest-exec-stop` runs the guest's first image through the
shared first-image launcher, which initializes the genuine in-guest Detcore
runtime, with its own private libc, before releasing the guest's first
instruction (Hermit 059e069bfae, 2026-10-20). `hermit oci run` applies the same
rule. A request is admitted only when all of these hold:

- Hermit was built with the `exec-stop-launcher` and `liteinst` Cargo features,
  and the private runtime package (its image and data resources) is installed;
- the backend is `in-guest-trap`;
- the run uses none of `--verify`, `--verify-run`, `--namespace-only`,
  `--strace-only`, `--dap-listen`, `--gdbserver`, or network recording, replay
  or perturbation.

Only the first image runs this way: guest threads, a later exec and unsupported
startup events are refused with named reasons when the guest reaches them.
Every other request fails with the typed policy refusal
`exec-stop-runtime-not-ready`. That refusal precedes host admission,
image-store access, stdin capture, container creation and guest execution, and
a requested verification file is first replaced with this invocation's
NO-RESULT record. Omitting the flag preserves the existing launch path.

The optional `exec-stop-launcher` Cargo feature exposes the shared launcher
dependency. The ordinary Hermit unit suite exercises it inside the existing
physical container. It prepares an explicit executable, argv, environment,
pinned working directory and borrowed bootstrap descriptors; it retains the
child's original pidfd/generation, reaches the kernel's exec stop, drains only
the kernel exec return, captures entry state and cancels with an observed
terminal wait. No guest instruction is released by this production interface.

The mapping and test-runtime handoff are available only in Reverie's separate
`stage1a-probe` feature. Passing that probe, including its static glibc smoke,
does not establish genuine Detcore readiness or production static support.

## Required before activation

The next integration must satisfy the named **production ToolRegion reservation
and authenticated adoption prerequisite**: permanently reserve the runtime and
loader image windows and guards in the real allocator, authenticate ownership
before adoption, and qualify the resulting runtime with M1. An address or a
record magic value alone is not ownership evidence. No existing guest mapping
may be replaced to make adoption succeed.

Real Detcore must initialize without executing guest libc or constructors and
arm interception before the original loader or static entry executes. Release
requires a genuine readiness transition; a fixture READY token cannot grant it.
Acceptance includes static no-libc and glibc-static programs, dynamic startup,
threads and exec re-entry, with full syscall, instruction-count, heap/stack-hash
and INFO/DETLOG parity. This commit does not waive or claim those gates.
