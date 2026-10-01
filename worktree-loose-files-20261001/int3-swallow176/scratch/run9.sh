#!/bin/bash
S=$PWD; H=$S/../hermit/target/debug/hermit; : > /tmp/nine.txt
run() { local be=$1 g=$2; shift 2
  setsid timeout -k 3 150 env -i PATH=/usr/bin:/bin HOME=/tmp TERM=dumb LC_ALL=C TZ=UTC \
    $H --backend $be run --strict --verify -- $S/g82/$g "$@" </dev/null >/tmp/n.o 2>/tmp/n.e
  local rc=$? v="OTHER"
  grep -q ":: Success: deterministic" /tmp/n.e && v="PASS_L2"
  grep -qiE "Mismatch|not deterministic" /tmp/n.e && v="DIVERGE"
  [ $rc -eq 124 ] || [ $rc -eq 137 ] && v="TIMEOUT"
  printf '%-9s %-42s args=%-14s rc=%-4s %s\n' "$be" "$g" "$*" "$rc" "$v" >> /tmp/nine.txt
}
for be in ptrace liteinst; do
  run $be c-programs_epoll-determinism multi
  run $be c-programs_mmap-determinism multiple
  run $be c-programs_socket-ioctl-timestamp v4-us
  run $be c-programs_ipc-determinism pipe
  run $be c-programs_signal-determinism raise
  run $be c-programs_thread-sync-determinism cancellation
  run $be determinism-stress-c_thread-contention contention
  run $be c-programs_liteinst-advanced threads 1
  cp "$S/../hermit/README.md" /tmp/fixture.txt 2>/dev/null
  run $be c-programs_record-replay-lseek-seek-cur /tmp/fixture.txt
done
echo DONE >> /tmp/nine.txt
