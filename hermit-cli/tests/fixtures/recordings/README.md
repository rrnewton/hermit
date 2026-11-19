# Checked-in recordings

`7a1b8833fde348c785270b61161d3a17/` is a genuine pre-split
(RECORD_VERSION 0x1a0) recording, made 2026-11-16 with a hermit
binary built from the last source before the x86-64-v2 psABI
split — the last whose x86-64-v2 was the Broadwell capture
(binary sha256
e72de9252ac617f822b380f29e9f976a53789d68b2037297bde98aa21a70fec7) —
running a static build of that source's leak-reporting CPUID
probe variant, `cpuid_v2_probe_leak.c` (the name the guest
binary itself embeds; today's
`tests/fixtures/isa/cpuid_v2_probe.c` is the same probe with
the seven checks as ordinary hidden assertions), under
`hermit record start
--cpu-profile=x86-64-v2 --base-env=minimal`, from a neutral
scratch directory, with the recording store and the guest
program also under that directory and the guest's stdout through
a pipe (the recorded stream kind is part of replay: recording
onto a seekable file makes replay seek a pipe). No
home-directory path, host
name, or inherited environment reaches any recorded field: the
guest environment is the synthetic minimal set, and the
recorded program path and working directory are the scratch
paths.

Its guest output depends on the seven CPUID bits of
https://github.com/facebookexperimental/hermit/issues/124, so it
was the bit-for-bit replay oracle for the legacy-table
migration: replayed on the legacy table it prints the seven
LEAK lines in
`7a1b8833fde348c785270b61161d3a17-expected-stdout.txt` byte for
byte; on the psABI table the same guest prints FIXED lines and
exits nonzero, so a broken migration cannot pass.

Since RECORD_VERSION 0x1a2, which changed the priority of a vfork
child under every older recording, replay refuses this recording
by its version, and the 0x1a1 replayer was the last to replay it.
It stays as the fixture for that refusal and for the psABI era's
table-digest gate, which its copies edited to the current version
exercise.

Nothing was normalized after recording: editing the recorded
program path breaks the recording's bootstrap exec request
(measured), so the scrub is in how it was recorded, not in an
edit after the fact. The checked-in copy omits only the
recording's top-level `exe` file, a convenience copy of the
guest program for interactive debugging: replay materializes
the guest from the content-addressed snapshot under
`exec_files/` (the same bytes), which is what is checked in —
point gdb at that snapshot for symbols. The test
`the_checked_in_pre_psabi_recording_carries_no_host_environment`
scans every file here for host paths, host names, and internal
names, and fails if a future re-record leaks them.

`b1bda700dbf34970882540c342894404/` is the same guest recorded
again at RECORD_VERSION 0x1a2, on 2026-11-17, with a hermit binary
built from the change that introduced 0x1a2 (binary sha256
40c89a7db5279ec3e5f5b101a6c957db7bcbf8f9a5214944782e0d652408b267),
so that a genuine recording at the current version is replayed bit
for bit (`a_genuine_current_version_recording_replays_bit_for_bit`).
The recipe is the one above, made exact:

1. Copy the guest out of the pre-split fixture's `exec_files/`
   snapshot (sha256 4587db6d...2479c, the static leak-reporting
   probe) to `/tmp/v2record/work/cpuid-v2-probe`, mode 755.
2. From `/tmp/v2record/work`, with `HERMIT_DATA_DIR=/tmp/v2record/store`,
   run `hermit record start --cpu-profile=x86-64-v2 --base-env=minimal
   -- /tmp/v2record/work/cpuid-v2-probe`, with the guest's stdout
   through a pipe into `<ID>-expected-stdout.txt`.
3. Check in the recording without its top-level `exe` file.

On the psABI table it records, the guest prints a FIXED line for
each of the seven bits and exits 1; replay reproduces both, and it
binds the table by the recorded `cpu_table_digest`. When
RECORD_VERSION moves, replay refuses this recording by its version:
record it again the same way in that change, and update its ID in
that test and in the host-environment scan.
