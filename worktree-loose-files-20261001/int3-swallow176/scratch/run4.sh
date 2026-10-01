#!/bin/bash
S=$PWD; H=$S/../hermit/target/debug/hermit; : > /tmp/four.txt
run() { local be=$1 g=$2 wd=$3; shift 3
  ( cd "$wd" && setsid timeout -k 3 150 env -i PATH=/usr/bin:/bin HOME=/tmp TERM=dumb LC_ALL=C TZ=UTC \
    $H --backend $be run --strict --verify -- $S/g82/$g "$@" </dev/null >/tmp/f.o 2>/tmp/f.e )
  local rc=$? v="OTHER"
  grep -q ":: Success: deterministic" /tmp/f.e && v="PASS_L2"
  grep -qiE "Mismatch|not deterministic" /tmp/f.e && v="DIVERGE"
  [ $rc -eq 124 ] || [ $rc -eq 137 ] && v="TIMEOUT"
  printf '%-9s %-42s args=%-16s rc=%-4s %s\n' "$be" "$g" "$*" "$rc" "$v" >> /tmp/four.txt
  [ "$v" = OTHER ] && head -c 220 /tmp/f.e | tr '\n' ' ' >> /tmp/four.txt && echo >> /tmp/four.txt
}
HRT="$S/../hermit"
for be in ptrace liteinst; do
  run $be c-programs_ipc-determinism           "$S"   pipe-order
  run $be c-programs_signal-determinism        "$S"   itimer-delivery
  run $be c-programs_liteinst-advanced         "$S"   threads
  run $be c-programs_record-replay-lseek-seek-cur "$HRT" README.md
done
echo DONE >> /tmp/four.txt
