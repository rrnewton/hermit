#!/usr/bin/env bash
# R/R corpus matrix runner. Mirrors ai_docs/transient/strict-compat-matrix.md.
# Runs `hermit record --verify` per case with a timeout; classifies PASS/FAIL/HANG.
set -u
HERMIT="${HERMIT_BIN:-$PWD/target/release/hermit}"
WORK="$PWD/rr-work"
TO="${CASE_TIMEOUT:-60}"
RESULTS="$WORK/results.tsv"

rm -rf "$WORK"; mkdir -p "$WORK/srcdir" "$WORK/data"
cd "$WORK"

# ---- fixtures (mirror the strict matrix) ----
printf 'alpha\nbeta\ngamma\ndelta\nepsilon\n' > lines.txt
cp lines.txt lines_copy.txt
printf 'banana\napple\ncherry\napple\n' > fruits.txt
printf 'apple\napple\nbanana\ncherry\n' > sorted.txt
printf 'one\ntwo\nthree\n' > srcdir/a.txt
printf 'four\nfive\n' > srcdir/b.txt
printf '10\n20\n30\n40\n' > nums.txt
printf 'foo and foo again\n' > text.txt
printf 'Hermit deterministic 53-byte data fixture: 0123456789ab\n' > DATA   # ~53 bytes
printf '{"a":1,"b":[2,3]}\n' > data.json
printf 'hello hash input\n' > hash-input.txt
printf 'deadbeef cafe\n' > hex-input.txt
printf '.globl _start\n_start:\n  mov $60, %%rax\n  xor %%rdi, %%rdi\n  syscall\n' > add.s
printf 'int main(void){return 0;}\n' > hello.c

# threads fixture (batch 4): pthread counter
cat > pth_counter.c <<'EOF'
#include <pthread.h>
#include <stdio.h>
static long c=0; static pthread_mutex_t m=PTHREAD_MUTEX_INITIALIZER;
void* w(void*_){for(int i=0;i<100000;i++){pthread_mutex_lock(&m);c++;pthread_mutex_unlock(&m);}return 0;}
int main(){pthread_t t[4];for(int i=0;i<4;i++)pthread_create(&t[i],0,w,0);for(int i=0;i<4;i++)pthread_join(t[i],0);printf("%ld\n",c);return 0;}
EOF
gcc -O2 -o pth_counter pth_counter.c -lpthread 2>/dev/null || echo "WARN: pth_counter build failed"

printf 'Program\tCommand\tCategory\tResult\tExit\tDetail\n' > "$RESULTS"

run() {  # name | category | command...
  local name="$1"; local cat="$2"; shift 2
  local dd="$WORK/rec_$$_$RANDOM"; mkdir -p "$dd"
  local out; local ec
  out=$(timeout --kill-after=5s "$TO" "$HERMIT" record --verify --data-dir "$dd" -- "$@" 2>&1)
  ec=$?
  local result detail
  if [ "$ec" -eq 124 ] || [ "$ec" -eq 137 ]; then
    result="HANG"; detail="timeout ${TO}s (ec=$ec)"
  elif [ "$ec" -eq 0 ] && printf '%s' "$out" | grep -q "replay matched recording"; then
    result="PASS"; detail="replay matched recording"
  else
    result="FAIL"; detail=$(printf '%s' "$out" | grep -iE 'differ|unexpected event|panic|abort|error|mismatch|Got unexpected' | head -1 | cut -c1-90)
    [ -z "$detail" ] && detail="ec=$ec (no match marker)"
  fi
  rm -rf "$dd"
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$name" "$*" "$cat" "$result" "$ec" "$detail" >> "$RESULTS"
  printf '%-22s %-8s ec=%-3s %s\n' "$name" "$result" "$ec" "$detail"
}

# ---- Category: single-process utils (expected R/R-friendly) ----
run wc          util   /usr/bin/wc lines.txt
run sort        util   /usr/bin/sort fruits.txt
run uniq        util   /usr/bin/uniq sorted.txt
run head        util   /usr/bin/head -3 lines.txt
run tail        util   /usr/bin/tail -3 lines.txt
run find        util   /usr/bin/find srcdir
run factor      math   /usr/bin/factor 123456
run seq         math   /usr/bin/seq 1 100
run expr        math   /usr/bin/expr 2 + 3
run dc          math   /usr/bin/dc -e "2 3 + p"
run od          math   /usr/bin/od -An -tx1 DATA
run hexdump     math   /usr/bin/hexdump -C DATA
run strings     math   /usr/bin/strings DATA
run file        math   /usr/bin/file DATA
run stat        math   /usr/bin/stat DATA
run du          math   /usr/bin/du -b DATA
run awk         text   /usr/bin/awk "{s+=\$1} END{print s}" nums.txt
run sed         text   /usr/bin/sed s/foo/bar/g text.txt
run xxd         text   /usr/bin/xxd hex-input.txt
run sha256sum   hash   /usr/bin/sha256sum DATA
run md5sum      hash   /usr/bin/md5sum DATA

# ---- Category: env/system single-process ----
run env         sys    /usr/bin/env
run printenv    sys    /usr/bin/printenv
run date        sys    /bin/date
run hostname    sys    /bin/hostname
run uname       sys    /bin/uname -a
run whoami      sys    /usr/bin/whoami

# ---- Category: pipes / multiprocess (R/R hazard) ----
run pipe_echo_wc  pipe /usr/bin/bash -c "echo hello | wc -c"
run pipe_base64   pipe /bin/sh -c "/usr/bin/base64 DATA | /usr/bin/base64 -d"
run pipe_cut_head pipe /usr/bin/bash -c "cut -d: -f1 /etc/passwd | head -5"

# ---- Category: interpreters ----
run py39        interp /usr/bin/python3.9 -c "import sys;print(sys.version_info[0])"
run bash_loop   interp /usr/bin/bash -c "for i in 1 2 3; do echo \$i; done"
run perl        interp /usr/bin/perl -e "print 1+2, qq(\n)"

# ---- Category: threads ----
[ -x ./pth_counter ] && run pth_counter thread ./pth_counter

# ---- Category: signals / edge cases ----
run kill0        sig   /usr/bin/bash -c "kill -0 \$\$"
run sigterm_trap sig   /usr/bin/bash -c "trap 'echo caught' SIGTERM; kill -TERM \$\$; echo done"
run sigpipe      sig   /usr/bin/bash -c "yes | head -100 >/dev/null; echo piped_ok"
run bg_wait      sig   /usr/bin/bash -c "sleep 0.01 & wait \$!"
run forkexec50   sig   /usr/bin/bash -c "for i in \$(seq 1 50); do /bin/true; done; echo loop_ok"
run pid_virt     sig   /usr/bin/bash -c "echo \$\$; echo \$PPID"
run ulimit       sig   /usr/bin/bash -c "ulimit -n"
run true_        sig   /bin/true

echo
echo "=== SUMMARY ==="
awk -F'\t' 'NR>1{c[$4]++} END{for(k in c) printf "%s=%d\n",k,c[k]}' "$RESULTS"
echo "results: $RESULTS"
