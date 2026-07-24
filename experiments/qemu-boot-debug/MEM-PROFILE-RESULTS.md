# hermit+QEMU memory profile (for parallelism planning)

Reproduce: `./experiments/qemu-boot-debug/mem-profile.sh` (override `MEMS="96M 128M"`).

Method: busybox initramfs boots the host's `/boot/vmlinuz` (6.17 fbk kernel)
under QEMU (`-accel tcg,thread=single -smp 1 -icount shift=0,sleep=off`), guest
`/init` sleeps ~20s while we sample `/proc/PID/status`. Each config run twice:
under hermit (`run --no-sequentialize-threads --preemption-timeout 1e10`, ptrace
backend) and bare QEMU. The shared host runs many other agents' qemu processes,
so "our" qemu is found by walking the launched PID's descendant tree — NOT by
pattern matching. Sample taken ~9-10s in; `VmHWM ≈ VmRSS` in every sample, so
the reading is at/near the boot high-water mark.

## Results (RSS = resident, the number that matters for parallelism)

| -m    | mode         | qemu RSS  | hermit RSS | combined RSS | qemu VSZ (virtual) |
|-------|--------------|-----------|------------|--------------|--------------------|
| 256M  | hermit+qemu  | 155.5 MiB | 6.8 MiB    | **162.3 MiB**| 665 MiB            |
| 256M  | bare qemu    | 160.0 MiB | —          | 160.0 MiB    | 1.53 GiB           |
| 128M  | hermit+qemu  | 153.1 MiB | 6.7 MiB    | **159.8 MiB**| 537 MiB            |
| 128M  | bare qemu    | 157.6 MiB | —          | 157.6 MiB    | 1.41 GiB           |
| 96M   | hermit+qemu  | 152.9 MiB | 6.8 MiB    | **159.7 MiB**| 505 MiB            |
| 96M   | bare qemu    | 157.4 MiB | —          | 157.4 MiB    | 1.38 GiB           |
| 80M   | both         | FAILED — below kernel boot floor (no console output)          |
| 64M   | both         | FAILED — below kernel boot floor (QEMU exits, 0 kernel lines) |

## Findings

1. **Host RSS is bounded by QEMU's boot working set (~150–160 MiB), NOT by guest
   `-m`.** 96M→128M→256M barely moves RSS (153→153→155 MiB hermit; 157→158→160
   bare). Guest RAM is demand-paged; boot only touches ~150 MiB (TCG code cache
   + QEMU internals + touched guest pages). Over-provisioning guest RAM is free
   in RSS until the guest actually writes it.

2. **Hermit overhead is negligible: ~+2 MiB net combined.** The hermit
   supervisor adds ~6.8 MiB, but qemu-under-hermit's own RSS is ~4–5 MiB *lower*
   than bare, so combined is only ~2 MiB over bare (162.3 vs 160.0 at 256M).

3. **VSZ differs hugely but is irrelevant.** Bare qemu reserves ~1.4–1.6 GiB
   virtual; under hermit qemu reserves ~0.5–0.66 GiB. Virtual reservation, not
   physical — does not constrain parallelism.

4. **Minimum viable guest RAM = 96M** for this kernel+initramfs (80M/64M do not
   boot). But host RSS at the 96M floor is still ~160 MiB combined (see #1).

## Parallelism guidance

- Budget **~160 MiB RSS per hermit+QEMU instance** (≈200 MiB with headroom).
- On this 754 GiB host, **RAM is not the binding constraint** — thousands would
  fit memory-wise. The limit is CPU: `tcg,thread=single` makes each instance a
  CPU-bound single thread that pins ~1 host core during boot. Practical
  concurrency ≈ number of available cores, not a memory ceiling.
