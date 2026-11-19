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
migration: the 0x1a1 replayer, which accepted 0x1a0 recordings,
replayed it on the legacy table and printed the recorded seven LEAK
lines byte for byte; on the psABI table the same guest prints FIXED
lines and exits nonzero. (That expected output was checked in until
no test read it any more.) Since RECORD_VERSION 0x1a4, whose recordings name
the signals their guest started with ignored, the replayer
accepts only its own version, so this recording is a genuine
older recording that replay refuses by name before its guest runs
(`a_genuine_pre_psabi_recording_is_refused_by_name`).

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
