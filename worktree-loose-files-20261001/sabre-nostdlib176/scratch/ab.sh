#!/bin/bash
# usage: ab.sh <bindir> <label> [sabre-loader-override]
BINDIR="$1"; LABEL="$2"; LOADER="$3"
BIN="$BINDIR/hermit"
export HERMIT_SABRE_BINARY="${LOADER:-$BINDIR/sabre}"
HROOT=/home/newton/work/dev-hermit/worktrees/sabre-nostdlib176/hermit
BUILD=/home/newton/work/dev-hermit/worktrees/sabre-nostdlib176/scratch/corpus
CORPUS=/home/newton/work/dev-hermit/compat-envelope/corpus/corpus-c.tsv
mkdir -p "$BUILD"; export LC_ALL=C TZ=UTC
DEFICIT="backend-parity-c/cpuid-probe c-programs/arch-prctl-determinism c-programs/clone c-programs/dbi-unsupported-syscall c-programs/fp-reduction-nondeterminism c-programs/hello-nostdlib c-programs/pread64-nostdlib c-programs/pselect6-simulation c-programs/racewrite-nostdlib c-programs/resource-determinism c-programs/sigpipe-siginfo c-programs/sigtimedwait-no-timeout c-programs/vforkexec"
REGRESS="c-programs/ptrace-attach-eperm c-programs/setitimer-determinism c-programs/futex-requeue-enosys c-programs/getcpu determinism-stress-c/signal-order c-programs/dbi-pid-virtualization c-programs/thread-self-procfs-handoff c-programs/nanosleep-threads-nocrash c-programs/get-robust-list-self c-programs/lsm-get-self-attr-enosys"
for id in $DEFICIT $REGRESS; do
  line=$(grep -h "^$id|" "$CORPUS" | head -1)
  prog=$(echo "$line"|cut -d'|' -f2); cflags=$(echo "$line"|cut -d'|' -f3); lane=$(echo "$line"|cut -d'|' -f5)
  key=${id//\//_}; cell="$BUILD/$key"; mkdir -p "$cell"; guest="$cell/guest"
  if [ ! -x "$guest" ]; then
    cc -std=c11 -O2 -g -Wall -Wextra -Werror $cflags "$HROOT/$prog" -o "$guest" 2>"$cell/cc.err" \
      || { echo "$id|$LABEL|BUILD-FAIL|-|-"; continue; }
  fi
  EXTRA=""; [ "$lane" = portable ] && EXTRA="--no-virtualize-cpuid --max-timeslice=disabled"
  t0=$(date +%s%3N)
  timeout 120 "$BIN" --backend sabre run --strict --verify $EXTRA --base-env minimal -e LC_ALL=C -e TZ=UTC -- "$guest" >"$cell/$LABEL.o" 2>"$cell/$LABEL.e"
  rc=$?; t1=$(date +%s%3N)
  case $rc in 0) o=pass;; 124) o=timeout;; *) o="diverge-exit$rc";; esac
  nod=no; grep -q "without the guest ever reaching the Detcore coordinator" "$cell/$LABEL.e" && nod=YES
  echo "$id|$LABEL|$o|$((t1-t0))|$nod"
done
