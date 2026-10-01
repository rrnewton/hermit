#!/bin/bash
# Robust: file-based capture + </dev/null so a forking guest cannot hold a pipe open.
HR="$PWD/../hermit"; mkdir -p g82; : > /tmp/li82_probe.txt
n=0
while IFS=$'\t' read -r id prog cf; do
  n=$((n+1)); out=g82/$(echo "$id" | tr '/' '_')
  if [ ! -x "$out" ]; then gcc -O2 $cf -o "$out" "$HR/$prog" 2>/dev/null; fi
  if [ ! -x "$out" ]; then printf '%s\tBUILD_FAIL\t\n' "$id" >> /tmp/li82_probe.txt; continue; fi
  setsid timeout -k 2 5 ./"$out" </dev/null >/tmp/pr.o 2>/tmp/pr.e
  rc=$?
  o=$(head -c 400 /tmp/pr.o /tmp/pr.e 2>/dev/null | tr '\n' ' ')
  if echo "$o" | grep -qiE "usage:?"; then cls="NEEDS_ARG"
  elif [ $rc -eq 124 ] || [ $rc -eq 137 ]; then cls="HANGS_BARE"
  elif [ $rc -eq 0 ]; then cls="RUNS_OK"
  else cls="EXITS_$rc"; fi
  printf '%s\t%s\t%s\n' "$id" "$cls" "$(echo "$o" | cut -c1-110)" >> /tmp/li82_probe.txt
done < /tmp/li82.tsv
echo -e "DONE\t$n\t" >> /tmp/li82_probe.txt
