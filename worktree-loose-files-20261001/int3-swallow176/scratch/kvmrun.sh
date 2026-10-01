#!/bin/bash
S=$PWD; H=$S/../hermit/target/debug/hermit
for spec in "epoll_determinism multi" "mmap_determinism multiple" "socket_ioctl_timestamp v4-us"; do
  set -- $spec; g=$1; a=$2
  ok=0
  for i in 1 2 3; do
    timeout 180 env -i PATH=/usr/bin:/bin HOME=/tmp TERM=dumb LC_ALL=C TZ=UTC \
      $H --backend kvm run --strict --verify -- $S/g/$g $a >/tmp/kvm_$g.out 2>/tmp/kvm_$g.err
    rc=$?; grep -q ":: Success: deterministic" /tmp/kvm_$g.err && ok=$((ok+1))
  done
  printf 'kvm %-24s args=%-9s L2 %d/3 lastrc=%s\n' "$g" "$a" "$ok" "$rc" >> /tmp/kvm_results.txt
done
echo DONE >> /tmp/kvm_results.txt
