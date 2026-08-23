# HANDOFF — fail-closed unsupported-syscall default
task: panic-on-unsupported-syscalls-default
branch: fail-closed-by-default   SHA: 4a744f168e93110efa73824aa4df8c3b0c0147d4
base: hermit main 4c70658e7 (rebased, zero conflicts)   PR: rrnewton/hermit#1677 (DRAFT)
STATE: pushed + PR open + remote-verified. NOT merged. No validate receipt (3 files of CLI
defaults; fold into another stack's box-exclusive ~528s validate rather than spending a slot).
DONE: panic_on_unsupported_syscalls defaults ON; --no-panic-on-unsupported-syscalls opt-out;
--strict refuses the opt-out; --passthru-opt implies the opt-out (without this the flip breaks
every --passthru-opt run); Display renders the opt-out. 5 unit assertions, build+tests green.
NEXT: fold into a stack validate, then land serially via det2.
GATE / KNOWN LIMIT: does NOT make the deterministic-refusal class abort loudly — guest gets a
fixed ENOSYS and can still exit 0. Control: --strict behaves identically, so pre-existing.
Routing that class to the abort path is a SEPARATE change.
