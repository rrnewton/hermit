# Demo 07: reproducible kernel evolution through drgn

Demo 07 consumes the QEMU/Linux boot snapshot published by Demo 5. It does not
boot Linux again. For each of two independent restores it performs this flow:

1. restore `hermit-boot` with the virtual CPUs paused;
2. freeze Hermit's exact tracer while QEMU is in a ptrace stop;
3. walk the guest Linux `task_struct.tasks` list through drgn;
4. resume and run one fixed guest interval which creates two persistent tasks
   and waits for 1000 microseconds of QEMU virtual time;
5. pause and freeze the same processes again; and
6. walk the task list a second time.

The second list must differ from the first. The two independent restores must
produce identical before lists, after lists, and task-list diffs.

drgn never attaches to QEMU. `demos/lib/drgn_hermit.py` registers a physical
memory callback backed by read-only `pread()` calls on
`/proc/<qemu-pid>/mem`. Each observation requires all three invariants below:

- QEMU remains in ptrace-stop state `t`;
- Hermit's exact tracer thread group remains stopped in state `T`; and
- the serial transcript grows by zero bytes.

The stopped tracer is the only process that can resume QEMU, so the two drgn
observations execute no guest instructions and consume no guest virtual time.
The explicit interval between them is different: it deliberately resumes the
guest under QEMU `-icount`, creates two long-lived `sleep` tasks, waits for a
fixed one-millisecond guest timer, and pauses at its serial completion marker.

## Run

First create the reusable boot snapshot once:

```bash
./demos/05-qemu-boot.py
```

Then run the two-restore evolution check:

```bash
./demos/07-drgn-kernel.sh
```

On a host which requires the outbound proxy for first-use asset provisioning:

```bash
with-proxy ./demos/05-qemu-boot.py
with-proxy ./demos/07-drgn-kernel.sh
```

The fixed public `bzImage` contains BTF. On first use Demo 07 extracts its
XZ-compressed ELF payload to `ignored/qemu-linux/vmlinux` and verifies that its
GNU BuildID matches the live guest's VMCOREINFO. The compressed boot ELF does
not retain an ELF symbol table, so Demo 07 registers the runtime `SYMBOL(...)`
entries from that same VMCOREINFO note with drgn before loading BTF types.

`DEMO07_SNAPSHOT_DISK` and `DEMO07_SNAPSHOT_NAME` select the phase-5 artifact.
`DEMO07_KERNEL`, `DEMO07_INITRD`, and `DEMO07_VMLINUX` may select another
matching kernel triplet. `QEMU_BIN`, `QEMU_ASSETS`, `HERMIT_RELEASE`,
`DEMO07_RUNS`, and `DEMO07_TIMEOUT` provide the remaining overrides.
