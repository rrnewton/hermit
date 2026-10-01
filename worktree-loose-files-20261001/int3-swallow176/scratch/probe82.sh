#!/bin/bash
HR="$PWD/../hermit"; mkdir -p g82; : > /tmp/li82_probe.txt
while IFS=$'\t' read -r id prog cf; do
  out=g82/$(echo "$id" | tr '/' '_')
  if gcc -O2 $cf -o "$out" "$HR/$prog" 2>/dev/null; then
    o=$(timeout -k 1 4 ./"$out" </dev/null 2>&1); rc=$?
    if echo "$o" | grep -qiE "usage:?"; then cls="NEEDS_ARG"
    elif [ $rc -eq 124 ] || [ $rc -eq 137 ]; then cls="HANGS_BARE"
    elif [ $rc -eq 0 ]; then cls="RUNS_OK"
    else cls="EXITS_$rc"; fi
    printf '%s\t%s\t%s\n' "$id" "$cls" "$(echo "$o" | head -1 | cut -c1-80)" >> /tmp/li82_probe.txt
  else
    printf '%s\tBUILD_FAIL\t\n' "$id" >> /tmp/li82_probe.txt
  fi
done < /tmp/li82.tsv
echo DONE >> /tmp/li82_probe.txt
