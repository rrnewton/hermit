#!/bin/bash
# Probe the PRODUCT condition, not the harness label.
#   GOOD = the kvm cell produced a passing comparison
#   BAD  = the kvm cell produced NO comparison, however the harness labels it
#          (FAIL "Signaled{14} ... no comparison was returned"  OR
#           ERROR "verification recorded no comparison at all (verdict=no_result)")
#   125  = could not build / no verdict line at all
cd /home/newton/work/dev-hermit/worktrees/bisect-si || exit 125
with-proxy git submodule update --init agent-utils >/dev/null 2>&1
with-proxy cargo build -p hermit-manifest-plan --bins >/dev/null 2>&1 || exit 125
with-proxy cargo build --bin hermit >/dev/null 2>&1 || exit 125
with-proxy ./target/debug/test-harness build --lane portable --test c-programs/setitimer-determinism >/dev/null 2>&1 || exit 125
out=$(with-proxy ./target/debug/test-harness run --lane portable --test c-programs/setitimer-determinism 2>&1)
kvm=$(echo "$out" | grep -E '^(ERROR|FAIL|PASS) c-programs/setitimer-determinism \(verify/kvm\)' | head -1)
sha=$(git rev-parse --short HEAD)
case "$kvm" in
  PASS*)        echo "PROBE $sha GOOD"; exit 0 ;;
  FAIL*|ERROR*) echo "PROBE $sha BAD  :: ${kvm:0:110}"; exit 1 ;;
  *)            echo "PROBE $sha UNTESTABLE (no kvm verdict line)"; exit 125 ;;
esac
