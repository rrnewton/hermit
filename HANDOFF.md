# HANDOFF — contract fixture
task: fixture-timer-family-identity
branch: fixture/timer-family-identity   SHA: 93310c3f9f1899372a1ea2aa3dc01204b7a970be
base: hermit main 4c70658e7   PR: rrnewton/hermit#1698 (DRAFT)
STATE: pushed + PR open + remote-verified. NOT merged. Assurance L1 (ptrace, relaxations none;
plus e9patch preprocessing on the ptrace runtime). NOT L2 - no --verify-strict witness taken.
DONE: timer families (timerfd/setitimer REAL+VIRTUAL/timer_create/alarm/epoll+futex timeouts), multi-threaded, asserts wake ORDER not durations
Plus a [[test]] entry in tests/e2e/manifests/backend-parity-c.toml (TOML re-parsed).
NEXT: re-run against DBI and FIX THE MANIFEST REASON STRING - it says dbi/sabre "cmake absent",
which is now FALSE (cmake is at ~/.local/bin/cmake via pip; --features third-party-backends
builds and DBI runs). Then fold into a stack validate and land serially via det2.
GATE: no validate receipt (test-only change; do not spend a box-exclusive slot alone).
