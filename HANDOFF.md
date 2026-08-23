# HANDOFF — contract fixture
task: fixture-socket-epoll-ordering-identity
branch: fixture/socket-epoll-ordering   SHA: 391c49e317f502073eb60dced4acdf00f670889d
base: hermit main 4c70658e7   PR: rrnewton/hermit#1701 (DRAFT)
STATE: pushed + PR open + remote-verified. NOT merged. Assurance L1 (ptrace, relaxations none;
plus e9patch preprocessing on the ptrace runtime). NOT L2 - no --verify-strict witness taken.
DONE: ephemeral-port selection + epoll readiness ORDERING; guest BRANCHES on which fd is ready first so the divergence propagates into the syscall sequence
Plus a [[test]] entry in tests/e2e/manifests/backend-parity-c.toml (TOML re-parsed).
NEXT: re-run against DBI and FIX THE MANIFEST REASON STRING - it says dbi/sabre "cmake absent",
which is now FALSE (cmake is at ~/.local/bin/cmake via pip; --features third-party-backends
builds and DBI runs). Then fold into a stack validate and land serially via det2.
GATE: no validate receipt (test-only change; do not spend a box-exclusive slot alone).
