# Demo 07: read guest kernel state without advancing virtual time

Demo 07 boots the fixed QEMU/Linux guest under Hermit's ptrace backend and
prints the first 16 entries in the live guest `task_struct.tasks` list. drgn is
not attached to QEMU. Instead, `demos/lib/drgn_hermit.py` registers a physical
memory callback backed by read-only `pread()` calls on
`/proc/<qemu-pid>/mem`.

Before exposing the drgn `Program`, the helper waits for QEMU to enter a ptrace
stop, resolves QEMU's actual `TracerPid` to the tracer thread-group ID, and
stops that exact Hermit thread group. Completion requires all three invariants:

- QEMU stayed in state `t`;
- Hermit's tracer stayed in state `T`; and
- the guest serial log grew by zero bytes.

The stopped tracer is the only process which can resume its tracee, so the
observation executes no QEMU or guest instructions and has zero guest
virtual-time cost. Teardown sends `SIGKILL` only to the process group created
for that observation.

## Run

Install the core and QEMU demo dependencies plus drgn, then run:

```bash
./demos/07-drgn-kernel.sh
```

On a host which requires an outbound proxy for first-use kernel provisioning:

```bash
with-proxy ./demos/07-drgn-kernel.sh
```

The fixed public `bzImage` contains BTF. On first use the helper extracts its
XZ-compressed ELF payload to `ignored/qemu-linux/vmlinux` and verifies that its
GNU BuildID exactly matches the live guest's VMCOREINFO before loading it.

For a built-in reproducibility check, boot two independent observations and
require the task rows and SHA-256 digest to match:

```bash
DEMO07_RUNS=2 ./demos/07-drgn-kernel.sh
```

`DEMO07_KERNEL`, `DEMO07_INITRD`, and `DEMO07_VMLINUX` may select another
kernel triplet. The kernel and vmlinux must match by BuildID. `QEMU_BIN`,
`QEMU_ASSETS`, `HERMIT_RELEASE`, `DEMO07_TASK_LIMIT`, and `DEMO07_TIMEOUT`
provide the remaining documented overrides.
