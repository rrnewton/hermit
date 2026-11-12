#!/usr/bin/env bash
# One isolated pinned-image build (runs a or b): fresh source snapshot, fresh
# Nix store, fresh Podman store, under the repin resource bound. Resource use
# is polled live (transient units are collected on exit, resetting the
# counters), so unit.txt records the last observed values.
set -euo pipefail
RUN=${1:?usage: run-build.sh a|b}
SLOT=/home/newton/work/dev-hermit/worktrees/slots/museops-20261028
BUILDBASE=/home/newton/temp/hermetic-gtest-build
WT=$SLOT/hermetic-gtest-wt
BUILD=$BUILDBASE/gtest-repin-$RUN
COMMIT=1c8aa13591b52ace5b0d65f3fb85a1de83de7dad
UNIT=hermetic-gtest-repin-$RUN

[[ ! -e $BUILD ]] || { echo "build root exists: $BUILD" >&2; exit 1; }
mkdir -p "$BUILD/src" "$BUILD/nix-root" "$BUILD/podman-data"
git -C "$WT" archive "$COMMIT" | tar -x -C "$BUILD/src"
mkdir -p "$BUILD/nix-root/nix"
{
    echo "source_snapshot_identity=src@$(stat -c '%d:%i' "$BUILD/src")"
    echo "nix_store_identity=nix-root/nix@$(stat -c '%d:%i' "$BUILD/nix-root/nix")"
    echo "podman_store_identity=gtest-repin-$RUN/podman-data"
    echo "run_id=$UNIT.service"
    echo "commit=$COMMIT"
} > "$BUILD/identities.txt"
cat "$BUILD/identities.txt"

systemd-run --user --unit="$UNIT" \
    -p CPUQuota=200% -p MemoryMax=8589934592 -p MemorySwapMax=0 \
    -p RuntimeMaxSec=2400 -p MemoryAccounting=yes \
    -p StandardOutput="file:$BUILD/service.stdout.log" \
    -p StandardError="file:$BUILD/service.stderr.log" \
    --setenv=PATH=/home/newton/.nix-profile/bin:/home/newton/bin:/usr/local/bin:/usr/bin:/bin \
    --setenv=NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt \
    --working-directory="$BUILD/src/ci/hermetic" \
    /home/newton/bin/with-proxy /home/newton/.nix-profile/bin/nix \
        --store "$BUILD/nix-root/nix" \
        --extra-experimental-features "nix-command flakes" \
        build --no-link --print-out-paths --option sandbox true \
        .#image
echo "unit started: $UNIT"

: > "$BUILD/unit-poll.txt"
while systemctl --user is-active --quiet "$UNIT.service" 2>/dev/null; do
    systemctl --user show "$UNIT.service" \
        -p Result -p ExecMainStatus -p MemoryPeak -p CPUUsageNSec \
        >> "$BUILD/unit-poll.txt" 2>/dev/null || true
    echo "---" >> "$BUILD/unit-poll.txt"
    sleep 5
done
systemctl --user show "$UNIT.service" \
    -p Result -p ExecMainStatus -p MemoryPeak -p CPUUsageNSec \
    >> "$BUILD/unit-poll.txt" 2>/dev/null || true
# Last observed values win; [not set] after collection falls back to the
# last live poll recorded above.
python3 - "$BUILD/unit-poll.txt" "$BUILD/unit.txt" <<'EOF'
import sys
vals = {}
for line in open(sys.argv[1]):
    if "=" in line and not line.startswith("---"):
        k, v = line.strip().split("=", 1)
        if v != "[not set]":
            vals[k] = v
with open(sys.argv[2], "w") as f:
    for k in ("Result", "ExecMainStatus", "MemoryPeak", "CPUUsageNSec"):
        f.write(f"{k}={vals[k]}\n")
print(open(sys.argv[2]).read())
EOF
grep -E '^(Result|ExecMainStatus)=' "$BUILD/unit.txt"
[[ $(grep '^Result=' "$BUILD/unit.txt") == "Result=success" ]] || {
    echo "build unit did not succeed" >&2
    tail -n 20 "$BUILD/service.stderr.log" >&2 || true
    exit 1
}
OUT_NAME=$(grep '^/nix/store/' "$BUILD/service.stdout.log" | tail -n 1 | xargs -r basename)
OUT=$BUILD/nix-root/nix/nix/store/$OUT_NAME
echo "output=$OUT" | tee "$BUILD/output.txt"
sha256sum "$OUT" | tee "$BUILD/archive.sha256"
stat -c 'archive_size_bytes=%s' "$OUT" | tee -a "$BUILD/output.txt"
podman --root "$BUILD/podman-data" load -i "$OUT" 2>&1 | tail -n 2
podman --root "$BUILD/podman-data" inspect \
    --format 'loaded_id={{.Id}} loaded_digest={{.Digest}}' \
    hermit-hermetic-validate:nix | tee "$BUILD/loaded.txt"
