#!/bin/bash
# Measure the SaBRe-unique deficit set at the current slot HEAD.
HROOT=/home/newton/work/dev-hermit/worktrees/sabre-nostdlib176/hermit
BIN=$HROOT/target/release/hermit
BUILD=/home/newton/work/dev-hermit/worktrees/sabre-nostdlib176/scratch/corpus
CORPUS=/home/newton/work/dev-hermit/compat-envelope/corpus/corpus-c.tsv
mkdir -p "$BUILD"
export LC_ALL=C TZ=UTC
CELLS="backend-parity-c/cpuid-probe c-programs/arch-prctl-determinism c-programs/clone c-programs/dbi-unsupported-syscall c-programs/fp-reduction-nondeterminism c-programs/hello-nostdlib c-programs/pread64-nostdlib c-programs/pselect6-simulation c-programs/racewrite-nostdlib c-programs/resource-determinism c-programs/sigpipe-siginfo c-programs/sigtimedwait-no-timeout c-programs/vforkexec"
echo "cell|lane|backend|rc|dur_ms|rpc|stderr_tail"
for id in $CELLS; do
  line=$(grep -h "^$id|" "$CORPUS" | head -1)
  prog=$(echo "$line" | cut -d'|' -f2); cflags=$(echo "$line" | cut -d'|' -f3); lane=$(echo "$line" | cut -d'|' -f5)
  key=${id//\//_}; cell="$BUILD/$key"; mkdir -p "$cell"; guest="$cell/guest"
  if [ ! -x "$guest" ]; then
    cc -std=c11 -O2 -g -Wall -Wextra -Werror $cflags "$HROOT/$prog" -o "$guest" 2>"$cell/cc.err" || { echo "$id|$lane|BUILD-FAIL|-|-|-|$(tail -1 $cell/cc.err)"; continue; }
  fi
  EXTRA=""; [ "$lane" = portable ] && EXTRA="--no-virtualize-cpuid --max-timeslice=disabled"
  for b in ptrace sabre; do
    BA=""; [ "$b" != ptrace ] && BA="--backend $b"
    t0=$(date +%s%3N)
    timeout 120 $BIN $BA --log info --log-file "$cell/$b.log" run --strict --verify $EXTRA --base-env minimal -e LC_ALL=C -e TZ=UTC -- "$guest" >"$cell/$b.o" 2>"$cell/$b.e"
    rc=$?; t1=$(date +%s%3N)
    rpc=$(grep -o 'guest_rpc_observed=[a-z]*' "$cell/$b.log" 2>/dev/null | tail -1 | cut -d= -f2)
    tail_e=$(grep -oE "SaBRe tracee terminated by a fatal signal|nondeterministic|Unsupported syscall|not deterministic|panicked" "$cell/$b.e" "$cell/$b.log" 2>/dev/null | head -1)
    echo "$id|$lane|$b|$rc|$((t1-t0))|${rpc:--}|${tail_e:-}"
  done
done
