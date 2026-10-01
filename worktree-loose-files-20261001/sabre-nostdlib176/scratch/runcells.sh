#!/bin/bash
# usage: runcells.sh <hermit-binary> <label> <cell...>
BIN="$1"; LABEL="$2"; shift 2
HROOT=/home/newton/work/dev-hermit/worktrees/sabre-nostdlib176/hermit
BUILD=/home/newton/work/dev-hermit/worktrees/sabre-nostdlib176/scratch/corpus
CORPUS=/home/newton/work/dev-hermit/compat-envelope/corpus/corpus-c.tsv
mkdir -p "$BUILD"; export LC_ALL=C TZ=UTC
for id in "$@"; do
  line=$(grep -h "^$id|" "$CORPUS" | head -1)
  prog=$(echo "$line" | cut -d'|' -f2); cflags=$(echo "$line" | cut -d'|' -f3); lane=$(echo "$line" | cut -d'|' -f5)
  key=${id//\//_}; cell="$BUILD/$key"; mkdir -p "$cell"; guest="$cell/guest"
  if [ ! -x "$guest" ]; then
    cc -std=c11 -O2 -g -Wall -Wextra -Werror $cflags "$HROOT/$prog" -o "$guest" 2>"$cell/cc.err" \
      || { printf "%-46s %-8s BUILD-FAIL\n" "$id" "$LABEL"; continue; }
  fi
  EXTRA=""; [ "$lane" = portable ] && EXTRA="--no-virtualize-cpuid --max-timeslice=disabled"
  t0=$(date +%s%3N)
  timeout 120 "$BIN" --backend sabre run --strict --verify $EXTRA --base-env minimal -e LC_ALL=C -e TZ=UTC -- "$guest" >"$cell/$LABEL.o" 2>"$cell/$LABEL.e"
  rc=$?; t1=$(date +%s%3N)
  case $rc in 0) o=pass;; 124) o=timeout;; *) o="fail-exit$rc";; esac
  nod=$(grep -c "without the guest ever reaching the Detcore coordinator" "$cell/$LABEL.e")
  printf "%-46s %-8s %-14s %7sms  no-detcore=%s\n" "$id" "$LABEL" "$o" "$((t1-t0))" "$nod"
done
