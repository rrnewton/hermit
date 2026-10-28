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

The `exec-stop-launcher` Cargo feature carries the shared launcher. It prepares
an explicit executable, argv, environment, pinned working directory and
borrowed bootstrap descriptors; retains the child's original pidfd/generation;
reaches the kernel's exec stop and drains only the kernel exec return. On the
admitted path, `RunOpts::run` opens the retained runtime image and
`run_mapped_in_container` hands it to `mapped_first_admission::run_mapped`
(`mapped_run.rs`), which maps the private runtime, initializes Detcore before
any guest instruction, and then releases the guest through `handover_status`.
Cleanup confirms the child's reap before any borrowed resource is released,
including after a normal guest exit. The launcher smoke test in the ordinary
Hermit unit suite stops short of that: it captures entry state and cancels
with an observed terminal wait, releasing no guest instruction.

Reverie's separate `stage1a-probe` feature keeps the earlier mapping and
test-runtime handoff probe. Passing it, including its static glibc smoke, is
not evidence about the production path.

## Qualification status

`mapped_run.rs` calls this path an intermediate opt-in whose full landing
qualification remains separate. The description of 059e069bfae reports what
was measured when it was activated: a private musl runtime owns its libc, TLS,
allocator and reserved Tool memory, so initialization does not run the guest
libc; and at Hermit 7b9f16cd with Reverie 1c5188a3 a static hello matched the
ptrace backend at all 15 syscall-entry boundaries and a dynamic hello at all
36, with matching INFO and memory records.

Full qualification is still the bar written before activation, and that
commit reports only the two hello images, not the whole of it:

- a **production ToolRegion reservation and authenticated adoption**: the
  runtime and loader image windows and guards permanently reserved in the real
  allocator, ownership authenticated before adoption, and the resulting runtime
  qualified with M1. An address or a record magic value alone is not ownership
  evidence, and no existing guest mapping may be replaced to make adoption
  succeed;
- release only through a genuine readiness transition; a fixture READY token
  cannot grant it;
- acceptance across static no-libc and glibc-static programs, dynamic startup,
  threads and exec re-entry, with full syscall, instruction-count,
  heap/stack-hash and INFO/DETLOG parity. Threads and exec re-entry are still
  refused today.
