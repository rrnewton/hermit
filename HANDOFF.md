# HANDOFF — contract fixture
task: fixture-stat-metadata-identity
branch: fixture/stat-metadata-identity   SHA: 3ab75f09cfbc543bc266cc8531982be0c7084799
base: hermit main 4c70658e7   PR: rrnewton/hermit#1707 (DRAFT)
STATE: pushed + PR open + remote-verified. NOT merged. Assurance L1 (ptrace, relaxations none;
plus e9patch preprocessing on the ptrace runtime). NOT L2 - no --verify-strict witness taken.
DONE: stat/fstat/lstat/statx metadata (ino, dev, timestamps, statx btime, nlink, blocks); anti-freeze check keys on st_mtim.tv_nsec because st_mtime granularity made it inert
Plus a [[test]] entry in tests/e2e/manifests/backend-parity-c.toml (TOML re-parsed).
NEXT: re-run against DBI and FIX THE MANIFEST REASON STRING - it says dbi/sabre "cmake absent",
which is now FALSE (cmake is at ~/.local/bin/cmake via pip; --features third-party-backends
builds and DBI runs). Then fold into a stack validate and land serially via det2.
GATE: no validate receipt (test-only change; do not spend a box-exclusive slot alone).
