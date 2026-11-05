#!/usr/bin/env bash
# Tests for scripts/run.sh. Safe to run on a host where the real vibe-talk is live: every check
# uses a throwaway image name and a throwaway port, and the only container it ever creates or
# stops is its own. It never builds a vibe-talk image and never touches port 8080.
#
#   scripts/test-run-sh.sh
#
# The container-detection checks need podman (or $VIBE_TALK_ENGINE); they are skipped, loudly,
# if it is not available.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
RUN_SH="$SCRIPT_DIR/run.sh"
ENGINE="${VIBE_TALK_ENGINE:-podman}"

# Deliberately distinctive so the no-leak check below cannot pass by accident.
SENTINEL_BOT_TOKEN="SENTINEL-BOT-TOKEN-9d41f0c2"
SENTINEL_READ_TOKEN="SENTINEL-READ-TOKEN-7b3ea15d"
SENTINEL_WRITE_TOKEN="SENTINEL-WRITE-TOKEN-2c98da44"
SENTINEL_EL_KEY="SENTINEL-ELEVENLABS-KEY-51ab7e30"

TEST_IMAGE_NAME="vibe-talk-selftest"
TEST_IMAGE_TAG="t0"
# A second throwaway image, built FROM scratch around a static binary, used to prove that the
# durable-state mount is writable by the container's user. Separate from the one above because it
# must run as uid 10001 and carry nothing else.
PROBE_IMAGE="vibe-talk-selftest-writeprobe:t0"
TEST_PORT=18099
# A tag and port that no test ever creates anything under, for the "nothing is there" checks.
ABSENT_TAG="no-such-tag"
ABSENT_PORT=18097
# run.sh names the container after the image, so the self-test's container is
# 'vibe-talk-selftest' and can never be the owner's 'vibe-talk'.
TEST_CONTAINER_NAME="$TEST_IMAGE_NAME"
# Printed by the test image at startup, so a log check cannot pass on empty output.
LOG_MARKER="VIBE-TALK-SELFTEST-LOG-MARKER-6f2ab913"

# A throwaway user unit of our own, used to prove --tunnel-status does not start what it reports
# on. It is never the owner's tunnel, and it is removed again in cleanup().
SELFTEST_UNIT="vibe-talk-selftest-tunnel.service"
SELFTEST_UNIT_PATH="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$SELFTEST_UNIT"

TMPDIR_TEST="$(mktemp -d)"
ALL_OUTPUT="$TMPDIR_TEST/all-output.txt"
: > "$ALL_OUTPUT"
CREATED_CONTAINER=""
# Every container this suite creates is tracked here, because cleanup() runs on FAILURE
# too. An untracked one survives a failed run and then matches the next run's image or
# port, which shows up as an unrelated check failing for a reason that is not there.
OLDSTYLE_CONTAINER=""
DECOY_CONTAINER=""

cleanup() {
    if [ -n "$CREATED_CONTAINER" ]; then
        "$ENGINE" rm -f "$CREATED_CONTAINER" >/dev/null 2>&1 || true
    fi
    if [ -n "$OLDSTYLE_CONTAINER" ]; then
        "$ENGINE" rm -f "$OLDSTYLE_CONTAINER" >/dev/null 2>&1 || true
    fi
    if [ -n "$DECOY_CONTAINER" ]; then
        "$ENGINE" rm -f "$DECOY_CONTAINER" >/dev/null 2>&1 || true
    fi
    "$ENGINE" rm -f "$TEST_CONTAINER_NAME" >/dev/null 2>&1 || true
    "$ENGINE" rmi "${TEST_IMAGE_NAME}:${TEST_IMAGE_TAG}" >/dev/null 2>&1 || true
    "$ENGINE" rmi "$PROBE_IMAGE" >/dev/null 2>&1 || true
    if [ -f "$SELFTEST_UNIT_PATH" ]; then
        systemctl --user stop "$SELFTEST_UNIT" >/dev/null 2>&1 || true
        rm -f "$SELFTEST_UNIT_PATH"
        systemctl --user daemon-reload >/dev/null 2>&1 || true
    fi
    rm -rf "$TMPDIR_TEST"
}
# INT, TERM and HUP as well as EXIT: an EXIT trap alone does not run when the shell is killed by a
# signal, and everything this suite creates -- containers, images, a user unit, a temp tree --
# outlives the run if cleanup is skipped.
trap cleanup EXIT INT TERM HUP

PASS=0
fail() { echo "FAILED: $*" >&2; exit 1; }
ok() { PASS=$((PASS + 1)); echo "ok   $*"; }

# Runs run.sh in a scrubbed environment (`env -i`), so no VIBE_TALK_* variable exported in this
# shell can leak into a test, and records every byte of output for the credential-leak check.
#
# HOME, XDG_RUNTIME_DIR and DBUS stay REAL on purpose: rootless podman and `systemctl --user`
# resolve their state through them. In particular, pointing XDG_CONFIG_HOME at a temp directory
# makes podman read a different, empty configuration and report ZERO running containers without
# any error — which would turn the already-running checks below into silent false passes. Tests
# that must not see the real ~/.config/vibe-talk/env therefore either pass --config (which
# replaces the file layer) or use run_sh_isolated_config, which does not need podman.
#
# That trap is why the tunnel-from-user-config checks below assert on run.sh's OWN error text
# rather than on anything podman reports.
#
# ENGINE_FOR_RUN lets a check hand run.sh a wrapper around podman (see FAKE_ENGINE below); it
# defaults to the real engine. The `timeout` is not decoration: a --logs or --follow that waits
# on a container that is not there would otherwise wedge the whole suite instead of failing, and
# "fails clearly rather than hanging" is one of the things under test. timeout exits 124.
TEST_XDG_CONFIG="$TMPDIR_TEST/config"
run_sh() {
    local out rc=0
    out="$(timeout 120 env -i \
            PATH="$PATH" \
            HOME="$HOME" \
            USER="${USER:-}" \
            XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-}" \
            DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-}" \
            VIBE_TALK_ENGINE="${ENGINE_FOR_RUN:-$ENGINE}" \
            bash "$RUN_SH" "$@" 2>&1)" || rc=$?
    [ "$rc" -ne 124 ] || fail "run.sh hung (timed out) on: $*"
    printf '%s\n' "$out" >> "$ALL_OUTPUT"
    printf '%s' "$out"
    return "$rc"
}

# Same, but with the per-user config directory redirected at a temp tree, for the checks about
# WHICH config file is read. Container detection is not meaningful under this one (see above).
run_sh_isolated_config() {
    local out rc=0
    out="$(env -i \
            PATH="$PATH" \
            HOME="$HOME" \
            USER="${USER:-}" \
            XDG_CONFIG_HOME="$TEST_XDG_CONFIG" \
            XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-}" \
            DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-}" \
            VIBE_TALK_ENGINE="$ENGINE" \
            bash "$RUN_SH" "$@" 2>&1)" || rc=$?
    printf '%s\n' "$out" >> "$ALL_OUTPUT"
    printf '%s' "$out"
    return "$rc"
}

mkdir -p "$TMPDIR_TEST/home" "$TEST_XDG_CONFIG"

write_config() {  # write_config FILE [omit-var]
    local file="$1" omit="${2:-}"
    : > "$file"
    [ "$omit" = VIBE_TALK_DISCORD_BOT_TOKEN ] || echo "VIBE_TALK_DISCORD_BOT_TOKEN=$SENTINEL_BOT_TOKEN" >> "$file"
    [ "$omit" = VIBE_TALK_READ_TOKEN ] || echo "VIBE_TALK_READ_TOKEN=$SENTINEL_READ_TOKEN" >> "$file"
    [ "$omit" = VIBE_TALK_WRITE_TOKEN ] || echo "VIBE_TALK_WRITE_TOKEN=$SENTINEL_WRITE_TOKEN" >> "$file"
    [ "$omit" = VIBE_TALK_CHANNELS ] || echo "VIBE_TALK_CHANNELS='123456789012345678:lead team:rw,987654321098765432:build noise:ro'" >> "$file"
    echo "VIBE_TALK_ELEVENLABS_API_KEY=$SENTINEL_EL_KEY" >> "$file"
    echo "VIBE_TALK_IMAGE_NAME=$TEST_IMAGE_NAME" >> "$file"
    echo "VIBE_TALK_IMAGE_TAG=$TEST_IMAGE_TAG" >> "$file"
    echo "VIBE_TALK_HOST_PORT=$TEST_PORT" >> "$file"
}

GOOD_CONFIG="$TMPDIR_TEST/good.env"
write_config "$GOOD_CONFIG"

echo "== run.sh tests =="

# --- 1. --help ------------------------------------------------------------------------------
out="$(run_sh --help)"
grep -q -- '--shutdown' <<< "$out" || fail "--help does not document --shutdown"
grep -q 'Configuration precedence' <<< "$out" || fail "--help does not explain config precedence"
grep -q 'EXIT' <<< "$out" || fail "--help does not say that --shutdown exits"
ok "--help documents the flags and the config precedence"

# The header comment IS the help text, so every flag the parser accepts must appear in it. Read
# the accepted flags out of the case statement rather than listing them here, so a flag added
# without a line of documentation fails this check instead of quietly shipping undocumented.
parsed_flags="$(sed -n '/^while \[ \$# -gt 0 \]/,/^done$/p' "$RUN_SH" \
    | grep -oE '^\s+(-[a-z]\|)?--[a-z-]+\)' | grep -oE -- '--[a-z-]+')"
[ -n "$parsed_flags" ] || fail "could not read the accepted flags out of run.sh's argument parser"
for flag in $parsed_flags; do
    grep -q -- "$flag" <<< "$out" || fail "--help does not document the accepted flag $flag"
done
ok "--help documents every flag the parser accepts ($(wc -w <<< "$parsed_flags") of them)"

# The new operator actions, and the restart-policy reasoning, must be IN the help: the owner is
# told to read --help rather than the source.
grep -q -- '--follow' <<< "$out" || fail "--help does not document --follow"
grep -q -- '--tunnel-status' <<< "$out" || fail "--help does not document --tunnel-status"
grep -q 'on-failure:5' <<< "$out" || fail "--help does not state the default restart policy"
grep -qi 'crash loop\|crash LOOP' <<< "$out" || fail "--help does not explain the crash-loop trade-off"
grep -q 'podman-restart.service' <<< "$out" || fail "--help does not say what reboot survival actually takes"
grep -qi 'READ-ONLY' <<< "$out" || fail "--help does not say --tunnel-status changes nothing"
ok "--help explains the two log actions, the restart-policy trade, and reboot survival"

# --- 2. no configuration at all -------------------------------------------------------------
# Run a COPY of run.sh from a temp directory, with an empty HOME and XDG_CONFIG_HOME, so the
# real ~/.config/vibe-talk/env is out of scope and no config file exists at all.
mkdir -p "$TMPDIR_TEST/isolated/scripts"
cp "$RUN_SH" "$TMPDIR_TEST/isolated/scripts/run.sh"
rc=0
out="$(env -i PATH="$PATH" HOME="$TMPDIR_TEST/home-empty" \
        XDG_CONFIG_HOME="$TMPDIR_TEST/config-empty" VIBE_TALK_ENGINE="$ENGINE" \
        bash "$TMPDIR_TEST/isolated/scripts/run.sh" --dry-run 2>&1)" || rc=$?
printf '%s\n' "$out" >> "$ALL_OUTPUT"
[ "$rc" -ne 0 ] || fail "missing config did not fail"
grep -q 'no configuration file found' <<< "$out" || fail "missing config error is not specific"
grep -q 'vibe-talk.env.example' <<< "$out" || fail "missing config error does not name the example file"
ok "missing configuration fails, naming the paths searched and the example to copy"

# --- 2b. a file at the REMOVED per-checkout path is not read --------------------------------
# vibe-talk/.vibe-talk.env used to be a config layer. It is not one any more. A complete, valid
# config sitting at that exact path next to run.sh must therefore change nothing: the run still
# fails for want of a config file. Asserting the failure (rather than just "it launched") is what
# makes this check impossible to pass by accident.
write_config "$TMPDIR_TEST/isolated/.vibe-talk.env"
rc=0
out="$(env -i PATH="$PATH" HOME="$TMPDIR_TEST/home-empty" \
        XDG_CONFIG_HOME="$TMPDIR_TEST/config-empty" VIBE_TALK_ENGINE="$ENGINE" \
        bash "$TMPDIR_TEST/isolated/scripts/run.sh" --dry-run 2>&1)" || rc=$?
printf '%s\n' "$out" >> "$ALL_OUTPUT"
[ "$rc" -ne 0 ] || fail "a config at the removed vibe-talk/.vibe-talk.env path was still honoured"
grep -q 'no configuration file found' <<< "$out" || fail "removed-overlay run failed for some other reason: $out"
grep -q '\.vibe-talk\.env' <<< "$out" && fail "run.sh still mentions the removed .vibe-talk.env path"
rm -f "$TMPDIR_TEST/isolated/.vibe-talk.env"
ok "a config file at the removed per-checkout path is ignored, and never mentioned"

# --- 3. --config pointing at a file that is not there ----------------------------------------
rc=0; out="$(run_sh --config "$TMPDIR_TEST/nope.env" --dry-run)" || rc=$?
[ "$rc" -ne 0 ] || fail "--config with a missing file did not fail"
grep -q 'not found' <<< "$out" || fail "--config missing-file error is not specific"
ok "--config with a missing file fails, naming the path"

# --- 4. a config that exists but is missing a required variable ------------------------------
for missing in VIBE_TALK_WRITE_TOKEN VIBE_TALK_CHANNELS; do
    bad="$TMPDIR_TEST/missing-$missing.env"
    write_config "$bad" "$missing"
    rc=0; out="$(run_sh --config "$bad" --dry-run)" || rc=$?
    [ "$rc" -ne 0 ] || fail "config missing $missing did not fail"
    grep -q "$missing" <<< "$out" || fail "error for missing $missing does not name the variable"
    ok "config missing $missing fails, naming that variable"
done

# --- 5. placeholder values still in place ----------------------------------------------------
ph="$TMPDIR_TEST/placeholder.env"
write_config "$ph"
sed -i "s/^VIBE_TALK_READ_TOKEN=.*/VIBE_TALK_READ_TOKEN=REPLACE-ME-at-least-24-characters/" "$ph"
rc=0; out="$(run_sh --config "$ph" --dry-run)" || rc=$?
[ "$rc" -ne 0 ] || fail "unedited placeholder value did not fail"
grep -q 'VIBE_TALK_READ_TOKEN' <<< "$out" || fail "placeholder error does not name the variable"
ok "an unedited REPLACE-ME value fails, naming that variable"

# --- 6. malformed channel list ---------------------------------------------------------------
badch="$TMPDIR_TEST/badchannels.env"
write_config "$badch"
sed -i "s/^VIBE_TALK_CHANNELS=.*/VIBE_TALK_CHANNELS='123456789012345678:lead team'/" "$badch"
rc=0; out="$(run_sh --config "$badch" --dry-run)" || rc=$?
[ "$rc" -ne 0 ] || fail "malformed VIBE_TALK_CHANNELS did not fail"
grep -q 'VIBE_TALK_CHANNELS entry is malformed' <<< "$out" || fail "malformed channels error is not specific"
ok "a malformed VIBE_TALK_CHANNELS entry fails before anything is built"

# --- 7. the per-user config location is the one that works ------------------------------------
mkdir -p "$TEST_XDG_CONFIG/vibe-talk"
cp "$GOOD_CONFIG" "$TEST_XDG_CONFIG/vibe-talk/env"
out="$(run_sh_isolated_config --dry-run --no-tunnel)"
grep -q 'Running' <<< "$out" || fail "user config did not reach the run step"
grep -q "config file: $TEST_XDG_CONFIG/vibe-talk/env" <<< "$out" \
    || fail "run.sh did not report reading the per-user config file"
ok "~/.config/vibe-talk/env alone is enough (the owner's current setup keeps working)"

# --- 7b. VIBE_TALK_TUNNEL_ENABLED is honoured FROM the per-user config file -------------------
# This flag lived only in the deleted per-checkout overlay, and losing it silently turned the
# tunnel check off. The per-user file is now the only place it can live, so prove it is read
# from there: not from the environment, not from --config, and not from a flag on the command
# line. The proof is that run.sh reaches the tunnel step and fails on a unit that cannot exist.
TUNNEL_CONFIG="$TEST_XDG_CONFIG/vibe-talk/env"
cp "$GOOD_CONFIG" "$TUNNEL_CONFIG"
echo "VIBE_TALK_TUNNEL_ENABLED=1" >> "$TUNNEL_CONFIG"
echo "VIBE_TALK_TUNNEL_UNIT=cloudflared-vibe-talk-not-a-real-unit.service" >> "$TUNNEL_CONFIG"
rc=0; out="$(run_sh_isolated_config --dry-run)" || rc=$?
[ "$rc" -ne 0 ] || fail "VIBE_TALK_TUNNEL_ENABLED=1 in the per-user config did not enable the tunnel check"
grep -q 'cloudflared-vibe-talk-not-a-real-unit.service' <<< "$out" \
    || fail "tunnel check ran but did not name the unit from the per-user config"
ok "VIBE_TALK_TUNNEL_ENABLED=1 in ~/.config/vibe-talk/env turns the tunnel check ON"

# Negative control: same file, flag flipped to 0. Without this, the check above would still pass
# if run.sh had started failing at the tunnel step for some reason unrelated to the flag.
sed -i "s/^VIBE_TALK_TUNNEL_ENABLED=.*/VIBE_TALK_TUNNEL_ENABLED=0/" "$TUNNEL_CONFIG"
out="$(run_sh_isolated_config --dry-run)"
grep -q 'Running' <<< "$out" || fail "tunnel flag 0 in the per-user config should have reached the run step"
grep -q 'systemctl' <<< "$out" && fail "tunnel flag 0 but the tunnel check still ran"
ok "VIBE_TALK_TUNNEL_ENABLED=0 in the same file turns it back OFF (the flag is what decides)"

cp "$GOOD_CONFIG" "$TUNNEL_CONFIG"

# --- 8. a good config reaches build and run, in dry run --------------------------------------
out="$(run_sh --config "$GOOD_CONFIG" --dry-run --no-tunnel)"
grep -q '(dry run).*build -t '"$TEST_IMAGE_NAME:$TEST_IMAGE_TAG" <<< "$out" || fail "dry run did not print the build command"
grep -q '(dry run).*run -d .*-p 127.0.0.1:'"$TEST_PORT" <<< "$out" || fail "dry run did not print the run command"
grep -q 'set: .*VIBE_TALK_DISCORD_BOT_TOKEN' <<< "$out" || fail "dry run does not report which variables are set"
ok "a valid config reaches build and run (dry run), reporting variables by name"

# --- 8b. the run command is detached, named, restart-policied, and NOT --rm ------------------
# --rm is what destroyed the access log of every crash: the container, and its logs with it,
# disappeared at exactly the moment there was something to read. Assert its ABSENCE, not just
# the presence of the new flags, or a stray --rm could come back unnoticed.
runline="$(grep '(dry run).*run -d' <<< "$out")"
grep -q -- '--name '"$TEST_IMAGE_NAME" <<< "$runline" || fail "run command does not name the container: $runline"
grep -q -- '--restart on-failure:5' <<< "$runline" || fail "run command does not set the default restart policy: $runline"
grep -q -- '--rm' <<< "$runline" && fail "run command still uses --rm, so the logs would not survive a stop: $runline"
ok "the launch is detached, named, restart-policied, and does not use --rm"

# --- 8c. durable state is bind-mounted from the host -----------------------------------------
# Without this mount the store lives in the container's writable layer, which run.sh itself
# destroys on the very next launch. That failure is silent: the server starts, writes happily,
# and the transcripts are simply gone after the next deploy. So assert the mount, not the
# VOLUME declaration in the Containerfile, which yields an anonymous volume nobody can find.
grep -q -- '-v .*:/var/lib/vibe-talk:Z' <<< "$runline" \
    || fail "run command does not bind-mount durable state, so a rebuild would discard it: $runline"
ok "durable state is bind-mounted from the host, so a rebuild does not discard it"

# --- 8d. the mount is writable BY THE CONTAINER'S USER, and the launcher checks it ------------
#
# The mount existing is not the property that matters; the server being able to WRITE to it is.
# The image runs as uid 10001, and under rootless podman a host directory owned by the invoking
# user and mode 0700 appears inside the container as root:root drwx------ — so uid 10001 is
# denied, the server refuses to start because it cannot open its SQLite file, and --restart turns
# that into a crash loop. This is the check that would have caught it.
DATA_PROBE_DIR="$TMPDIR_TEST/data"
mkdir -p -m 700 "$DATA_PROBE_DIR"
MOUNT_CONFIG="$TMPDIR_TEST/mount.env"
write_config "$MOUNT_CONFIG"
echo "VIBE_TALK_DATA_DIR=$DATA_PROBE_DIR" >> "$MOUNT_CONFIG"
mountout="$(run_sh --config "$MOUNT_CONFIG" --dry-run --no-tunnel)"
mountline="$(grep '(dry run).*run -d' <<< "$mountout")"
grep -q -- "-v ${DATA_PROBE_DIR}:/var/lib/vibe-talk:Z" <<< "$mountline" \
    || fail "VIBE_TALK_DATA_DIR is not the directory that gets mounted: $mountline"
grep -qi 'writable by the container' <<< "$mountout" \
    || fail "the launcher does not preflight the mount at all, so a store it cannot open is \
discovered inside the container: $mountout"
ok "the launcher mounts VIBE_TALK_DATA_DIR and says it will check the container can write there"

if ! command -v "$ENGINE" >/dev/null 2>&1; then
    echo "SKIP $ENGINE is not installed; the mount-writability check needs it" >&2
else
    ROOTLESS_HOST="$("$ENGINE" info --format '{{.Host.Security.Rootless}}' 2>/dev/null || true)"
    if [ "$ROOTLESS_HOST" = "true" ]; then
        # THE FUNCTIONAL CHECK FIRST, deliberately. Asserting that a flag appears in a command
        # line only proves the string is there; what has to be true is that a container running
        # as uid 10001 can WRITE to this mount, and only running one says so. It uses the very
        # arguments run.sh printed, and it is built FROM scratch around a static binary compiled
        # here — no registry, no network, no base image on the host.
        PROBE_CTX="$TMPDIR_TEST/probe-image"
        mkdir -p "$PROBE_CTX"
        cat > "$PROBE_CTX/probe.c" <<'PROBEC'
#include <fcntl.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc < 2) return 2;
    int fd = open(argv[1], O_CREAT | O_WRONLY | O_TRUNC, 0600);
    if (fd < 0) return 1;
    close(fd);
    return 0;
}
PROBEC
        if ! command -v cc >/dev/null 2>&1 \
            || ! cc -static -O0 -o "$PROBE_CTX/probe" "$PROBE_CTX/probe.c" 2>/dev/null; then
            echo "SKIP no static C compiler; cannot build the offline write-probe image" >&2
        else
            printf 'FROM scratch\nCOPY probe /probe\nUSER 10001:10001\nENTRYPOINT ["/probe"]\n' \
                > "$PROBE_CTX/Containerfile"
            "$ENGINE" build --pull=never -t "$PROBE_IMAGE" "$PROBE_CTX" >/dev/null 2>&1 \
                || fail "could not build the throwaway write-probe image"

            mount_arg="$(grep -o -- '-v [^ ]*' <<< "$mountline" | head -1 | cut -d' ' -f2)"
            userns_arg="$(grep -o -- '--userns=[^ ]*' <<< "$mountline" | head -1)"
            [ -n "$mount_arg" ] || fail "could not read the mount argument back out of: $mountline"

            "$ENGINE" run --rm -v "$mount_arg" "$userns_arg" "$PROBE_IMAGE" \
                /var/lib/vibe-talk/written-by-the-container >/dev/null 2>&1 \
                || fail "uid 10001 CANNOT write to the mount run.sh builds: the server would fail \
to open its store and crash-loop under --restart. args: -v $mount_arg $userns_arg"
            [ -f "$DATA_PROBE_DIR/written-by-the-container" ] \
                || fail "the container reported success but nothing landed in $DATA_PROBE_DIR"
            ok "a container running as uid 10001 really can write to the mount run.sh builds"

            # THE CONTROL. Without the remapping the same write is REFUSED on this host — which is
            # what makes the check above about the flag rather than about a permissive host.
            if "$ENGINE" run --rm -v "$mount_arg" "$PROBE_IMAGE" \
                    /var/lib/vibe-talk/control >/dev/null 2>&1; then
                fail "the control PASSED: this host allows uid 10001 to write to the bare mount, \
so the check above certifies nothing"
            fi
            ok "and without that mapping the identical write is refused, so the flag is load-bearing"

            # Said in the command line too, by name, so the reason the write above worked is
            # recorded where a reader of the launch output can see it.
            grep -q -- '--userns=keep-id:uid=10001,gid=10001' <<< "$mountline" \
                || fail "the write worked, but not because of the mapping this asserts: $mountline"
            ok "under a rootless engine the launch maps the container's server user onto this user"

            # Removed HERE, not only in cleanup(): the detection checks further down pick an
            # arbitrary local image to build their throwaway one from, and a FROM-scratch image
            # with no shell in it is not a usable base. Leaving it around made an unrelated check
            # fail for a reason that was not there.
            "$ENGINE" rmi "$PROBE_IMAGE" >/dev/null 2>&1 || true
        fi
    else
        echo "SKIP engine is not rootless; the userns remapping check does not apply" >&2
    fi
fi

# --status must say where that directory is, or an operator cannot find it to back it up or
# purge it.
statusout="$(run_sh --config "$GOOD_CONFIG" --status --no-tunnel)"
grep -q 'durable state:' <<< "$statusout" || fail "--status does not report the state directory: $statusout"
grep -q '/var/lib/vibe-talk' <<< "$statusout" || fail "--status does not say where state is mounted: $statusout"
ok "--status reports where durable state lives on the host"

# --restart-policy is honoured, and a bad one is refused BEFORE anything is built or stopped.
out="$(run_sh --config "$GOOD_CONFIG" --dry-run --no-tunnel --restart-policy always)"
grep -q -- '--restart always' <<< "$out" || fail "--restart-policy always was not applied"
ok "--restart-policy overrides the default"

rc=0; out="$(run_sh --config "$GOOD_CONFIG" --dry-run --no-tunnel --restart-policy sometimes)" || rc=$?
[ "$rc" -ne 0 ] || fail "an invalid restart policy was accepted"
grep -q 'sometimes' <<< "$out" || fail "invalid-policy error does not name the value given"
grep -q 'build -t' <<< "$out" && fail "invalid restart policy was only caught after the build step"
ok "an invalid restart policy fails early, naming the value and the valid ones"

# --build-network, and the two halves of it that matter. The default must stay ABSENT rather than
# becoming some explicit mode, because "whatever the engine chooses" is what every host that works
# today is relying on; and the flag must reach the BUILD and not the run, since the container is
# published on a port and has no business on the host's network namespace.
out="$(run_sh --config "$GOOD_CONFIG" --dry-run --no-tunnel)"
grep '(dry run).*build -t' <<< "$out" | grep -q -- '--network' \
    && fail "the build names a network mode by default; it must leave the engine's default alone"
ok "the build passes no network mode unless asked, leaving the engine's default alone"

out="$(run_sh --config "$GOOD_CONFIG" --dry-run --no-tunnel --build-network host)"
grep '(dry run).*build -t' <<< "$out" | grep -q -- '--network host' \
    || fail "--build-network host did not reach the build command: $out"
grep '(dry run).*run -d' <<< "$out" | grep -q -- '--network' \
    && fail "--build-network leaked into the run command, which must stay on its published port"
ok "--build-network reaches the build only, never the launched container"

# Two actions at once is a mistake, and silently honouring one of them is the worst answer.
rc=0; out="$(run_sh --config "$GOOD_CONFIG" --status --logs)" || rc=$?
[ "$rc" -ne 0 ] || fail "--status --logs together did not fail"
grep -q 'cannot be combined' <<< "$out" || fail "combined-actions error is not specific: $out"
ok "two actions at once is refused, naming both"

# --- 9. tunnel check, both settings ----------------------------------------------------------
out="$(run_sh --config "$GOOD_CONFIG" --dry-run --no-tunnel)"
grep -q 'systemctl' <<< "$out" && fail "tunnel disabled but systemd was still mentioned"
ok "tunnel disabled: skipped silently"

nounit="$TMPDIR_TEST/nounit.env"
write_config "$nounit"
echo "VIBE_TALK_TUNNEL_UNIT=cloudflared-vibe-talk-does-not-exist.service" >> "$nounit"
rc=0; out="$(run_sh --config "$nounit" --dry-run --tunnel)" || rc=$?
[ "$rc" -ne 0 ] || fail "tunnel enabled with a nonexistent unit did not fail"
grep -q 'cloudflared-vibe-talk-does-not-exist.service' <<< "$out" || fail "nonexistent-unit error does not name the unit"
grep -qi 'does not exist' <<< "$out" || fail "nonexistent-unit error is not a clear message"
ok "tunnel enabled with a missing unit: clear error naming the unit, not a crash"

# --- 9b. --tunnel-status ---------------------------------------------------------------------
# It must REPORT and change nothing. The unit named here cannot exist, so the report has to say
# so rather than crashing or, worse, trying to create or start something.
rc=0; out="$(run_sh --config "$nounit" --tunnel-status)" || rc=$?
[ "$rc" -ne 0 ] || fail "--tunnel-status on a missing unit exited 0 (it must report not-active)"
grep -q 'cloudflared-vibe-talk-does-not-exist.service' <<< "$out" || fail "--tunnel-status does not name the unit"
grep -qi 'NOT INSTALLED' <<< "$out" || fail "--tunnel-status does not say the unit is absent: $out"
grep -qi 'read-only' <<< "$out" || fail "--tunnel-status does not state that it changes nothing"
grep -q 'systemctl --user enable --now' <<< "$out" || fail "--tunnel-status does not say how to install the unit"
# It must not have gone on to build, run, or check anything else.
grep -q 'build -t' <<< "$out" && fail "--tunnel-status went on to build"
grep -q 'Running' <<< "$out" && fail "--tunnel-status went on to launch"
ok "--tunnel-status on an absent unit reports it, exits nonzero, and does nothing else"

# An INSTALLED but INACTIVE unit. This is the check that really pins "read-only": a unit that is
# already active stays active whether or not something calls `systemctl start` on it, so only an
# inactive one can prove the command did not start it. The unit is ours, uniquely named, and
# removed in cleanup — the owner's tunnel is never used as the guinea pig.
if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
    mkdir -p "$(dirname "$SELFTEST_UNIT_PATH")"
    cat > "$SELFTEST_UNIT_PATH" <<'EOF'
[Unit]
Description=vibe-talk self-test placeholder (safe to delete)
[Service]
Type=simple
ExecStart=/bin/sleep 600
EOF
    systemctl --user daemon-reload
    systemctl --user is-active --quiet "$SELFTEST_UNIT" \
        && fail "test setup: the placeholder unit was already active"

    inactive="$TMPDIR_TEST/inactive-unit.env"
    write_config "$inactive"
    echo "VIBE_TALK_TUNNEL_UNIT=$SELFTEST_UNIT" >> "$inactive"
    rc=0; out="$(run_sh --config "$inactive" --tunnel-status)" || rc=$?
    [ "$rc" -ne 0 ] || fail "--tunnel-status on an inactive unit exited 0"
    grep -q 'unit:      installed' <<< "$out" || fail "--tunnel-status did not see the installed unit: $out"
    grep -q 'active:    NO' <<< "$out" || fail "--tunnel-status did not report the unit as inactive: $out"
    grep -q "systemctl --user start $SELFTEST_UNIT" <<< "$out" || fail "--tunnel-status does not say how to start it: $out"
    systemctl --user is-active --quiet "$SELFTEST_UNIT" \
        && fail "--tunnel-status STARTED the unit it was only asked to report on"
    ok "--tunnel-status reports an installed-but-inactive unit and provably does not start it"

    systemctl --user stop "$SELFTEST_UNIT" >/dev/null 2>&1 || true
    rm -f "$SELFTEST_UNIT_PATH"
    systemctl --user daemon-reload
else
    echo "SKIP no usable 'systemctl --user': the inactive-unit check was not run" >&2
fi

# The real unit, when it is active. The control that matters is that the unit's
# ActiveEnterTimestamp is IDENTICAL before and after: a report that restarted the tunnel would
# still print "active", so "it says active" proves nothing on its own.
REAL_UNIT="cloudflared-vibe-talk.service"
if command -v systemctl >/dev/null 2>&1 && systemctl --user is-active --quiet "$REAL_UNIT"; then
    since_before="$(systemctl --user show "$REAL_UNIT" -p ActiveEnterTimestamp --value)"
    realunit="$TMPDIR_TEST/realunit.env"
    write_config "$realunit"
    echo "VIBE_TALK_TUNNEL_UNIT=$REAL_UNIT" >> "$realunit"
    out="$(run_sh --config "$realunit" --tunnel-status)"
    since_after="$(systemctl --user show "$REAL_UNIT" -p ActiveEnterTimestamp --value)"
    grep -q 'active:    YES' <<< "$out" || fail "--tunnel-status did not report the live unit as active: $out"
    grep -q "$REAL_UNIT" <<< "$out" || fail "--tunnel-status did not name the live unit"
    grep -qE 'since: +[A-Za-z]' <<< "$out" || fail "--tunnel-status did not report since-when: $out"
    grep -qE 'hostname: +[a-z0-9.-]+\.[a-z]{2,}' <<< "$out" || fail "--tunnel-status did not report a tunnel hostname: $out"
    [ "$since_before" = "$since_after" ] \
        || fail "--tunnel-status changed the unit's ActiveEnterTimestamp: it restarted the tunnel"
    systemctl --user is-active --quiet "$REAL_UNIT" || fail "--tunnel-status left the tunnel not active"
    ok "--tunnel-status reports the live unit (active, since, hostname) and provably does not restart it"

    # Negative control for the hostname line: with the ingress config pointed somewhere empty,
    # the same command must say the hostname is unknown rather than printing a stale one.
    hostless="$TMPDIR_TEST/hostless.env"
    write_config "$hostless"
    echo "VIBE_TALK_TUNNEL_UNIT=$REAL_UNIT" >> "$hostless"
    echo "VIBE_TALK_TUNNEL_CONFIG=$TMPDIR_TEST/no-such-cloudflared.yml" >> "$hostless"
    out="$(run_sh --config "$hostless" --tunnel-status)"
    grep -q 'hostname:  unknown' <<< "$out" || fail "a missing tunnel config did not make the hostname unknown: $out"
    ok "the hostname really comes from the tunnel config (unknown when that file is not there)"
else
    echo "SKIP $REAL_UNIT is not active: the live-tunnel checks were not run" >&2
fi

# --- 9c. --logs and --follow with nothing to read ---------------------------------------------
# Under a tag and port nothing in this suite ever creates, so the answer cannot be an accident.
# The `timeout` inside run_sh turns a hang into a failure; here we assert the clear message.
for action in --logs --follow; do
    rc=0; out="$(run_sh --config "$GOOD_CONFIG" --tag "$ABSENT_TAG" --port "$ABSENT_PORT" "$action")" || rc=$?
    [ "$rc" -ne 0 ] || fail "$action with no container exited 0"
    grep -q 'no vibe-talk container to read logs from' <<< "$out" || fail "$action error is not specific: $out"
    grep -q "$TEST_IMAGE_NAME:$ABSENT_TAG" <<< "$out" || fail "$action error does not name the image looked for: $out"
    grep -q "$ABSENT_PORT" <<< "$out" || fail "$action error does not name the port looked for: $out"
    ok "$action with no container fails clearly, naming what it looked for"
done

# --- 9d. --smoke-agent: the paths that must NOT open a billed conversation ---------------------
# Every check here is about REFUSING to run, so none of them can cost anything. The one thing this
# action must never do is start a conversation the operator did not mean to start.

out="$(run_sh --help)"
grep -qi 'COSTS VENDOR MINUTES' <<< "$out" || fail "--help does not warn that --smoke-agent costs money"
grep -qi 'MANUAL ONLY\|manual' <<< "$out" || fail "--help does not say --smoke-agent is manual"
grep -q '2026-08-19' <<< "$out" || fail "--help does not say which failure --smoke-agent exists to catch"
ok "--help says --smoke-agent costs vendor minutes, is manual, and why it exists"

# The owner asked explicitly for this not to be expensive, and a FAILING run costs about twice a
# passing one because it escalates to a second conversation. That has to be in the help, or the
# cost is a surprise discovered from a bill.
grep -qi 'escalat' <<< "$out" || fail "--help does not mention the automatic escalation"
grep -qi 'TWICE' <<< "$out" || fail "--help does not say that a failing run costs about twice a passing one"
grep -qi 'REFUSED' <<< "$out" || fail "--help does not say some runs are refused rather than decided"
ok "--help states what a passing run costs, what a failing one costs, and when a run is refused"

# --nonce alone is meaningless. Accepting it silently would leave the operator believing they had
# run the strong, proving check when they had run the weaker one.
rc=0; out="$(run_sh --config "$GOOD_CONFIG" --nonce)" || rc=$?
[ "$rc" -ne 0 ] || fail "--nonce on its own was accepted"
grep -q -- '--smoke-agent' <<< "$out" || fail "the --nonce refusal does not name the flag it belongs to: $out"
ok "--nonce without --smoke-agent is refused, naming the action it modifies"

rc=0; out="$(run_sh --config "$GOOD_CONFIG" --smoke-agent --status)" || rc=$?
[ "$rc" -ne 0 ] || fail "--smoke-agent --status together did not fail"
grep -q 'cannot be combined' <<< "$out" || fail "combined-actions error does not cover --smoke-agent: $out"
ok "--smoke-agent cannot be combined with another action"

# Nothing running means nothing to converse with AND no access log to check the agent against —
# and the access-log check is the primary assertion, so a run without it would be worse than no
# run at all.
rc=0; out="$(run_sh --config "$GOOD_CONFIG" --tag "$ABSENT_TAG" --port "$ABSENT_PORT" --smoke-agent)" || rc=$?
[ "$rc" -ne 0 ] || fail "--smoke-agent with nothing running exited 0"
grep -qi 'nothing is running' <<< "$out" || fail "--smoke-agent with nothing running is not specific: $out"
grep -q "$ABSENT_PORT" <<< "$out" || fail "--smoke-agent does not name the port it looked at: $out"
ok "--smoke-agent with nothing running refuses, naming what it looked for"

NO_WRITE_CONFIG="$TMPDIR_TEST/no-write.env"
write_config "$NO_WRITE_CONFIG" VIBE_TALK_WRITE_TOKEN
rc=0; out="$(run_sh --config "$NO_WRITE_CONFIG" --smoke-agent --dry-run)" || rc=$?
[ "$rc" -ne 0 ] || fail "--smoke-agent without a write token exited 0"
grep -q 'VIBE_TALK_WRITE_TOKEN' <<< "$out" || fail "--smoke-agent does not name the missing variable: $out"
ok "--smoke-agent without a write token refuses, naming the variable"

# The smoke test's OWN controls, run here so they are part of a suite the owner already runs.
# They are offline and cost nothing; if they ever fail, neither of the smoke test's assertions
# can be trusted and a green smoke run would mean nothing.
rc=0; out="$(timeout 60 python3 "$SCRIPT_DIR/smoke-agent.py" --self-test 2>&1)" || rc=$?
printf '%s\n' "$out" >> "$ALL_OUTPUT"
[ "$rc" -eq 0 ] || fail "the smoke test's own controls FAILED, so its assertions cannot be trusted: $out"
grep -q 'REJECTS a fluent confabulated reply' <<< "$out" \
    || fail "the smoke test's controls did not include the confabulation negative control: $out"
grep -q 'handshake-only log' <<< "$out" \
    || fail "the smoke test's controls did not include the no-tool-call negative control: $out"
ok "the smoke test's own controls pass (both negative controls included)"

# --- 9e. --screenshots: the capture harness, and the ports it must never take ------------------
#
# The screenshot action is the only check in this project that can SEE the page. Everything here
# is offline and free; none of it starts a browser or takes a picture. What it guards is the two
# ways this action could do harm or lie: binding a port that is serving something real, and
# writing pictures that are not what they say they are.

out="$(run_sh --help)"
grep -q -- '--screenshots' <<< "$out" || fail "--screenshots is not in the help text: $out"
grep -q 'FREE and offline' <<< "$out" \
    || fail "the help text does not say --screenshots costs nothing: $out"
grep -q -- '--out DIR' <<< "$out" || fail "--out is not documented in the help text: $out"
ok "--screenshots and --out are documented in the help text"

rc=0; out="$(run_sh --screenshots --shutdown)" || rc=$?
[ "$rc" -ne 0 ] || fail "--screenshots combined with --shutdown exited 0"
grep -q 'cannot be combined' <<< "$out" \
    || fail "--screenshots with --shutdown does not report an action conflict: $out"
ok "--screenshots refuses to be combined with another action"

rc=0; out="$(run_sh --out "$TMPDIR_TEST/shots")" || rc=$?
[ "$rc" -ne 0 ] || fail "--out on its own exited 0"
grep -q -- '--out only means something together with --screenshots' <<< "$out" \
    || fail "--out on its own does not say what it needs: $out"
ok "--out on its own refuses, naming the flag it belongs to"

# THE important one. 8080 is the owner's live agent; binding it to take pictures of a fake would
# take down the real thing. The refusal must name the port, not merely fail to bind.
rc=0; out="$(run_sh --screenshots --port 8080)" || rc=$?
[ "$rc" -ne 0 ] || fail "--screenshots --port 8080 exited 0 — it would contend with the LIVE server"
grep -q '8080' <<< "$out" || fail "--screenshots --port 8080 does not name the port: $out"
grep -q 'LIVE vibe-talk' <<< "$out" \
    || fail "--screenshots --port 8080 does not say what is on that port: $out"
ok "--screenshots refuses port 8080, naming the live deployment"

rc=0; out="$(run_sh --screenshots --port 18081)" || rc=$?
[ "$rc" -ne 0 ] || fail "--screenshots --port 18081 exited 0 — that is the ci container"
grep -q '18081' <<< "$out" || fail "--screenshots --port 18081 does not name the port: $out"
ok "--screenshots refuses port 18081, naming the ci container"

# The default must be neither of those, and the plan must say so out loud rather than leaving the
# reader to trust it.
out="$(run_sh --screenshots --dry-run)"
grep -q '127.0.0.1:18091' <<< "$out" \
    || fail "--screenshots --dry-run does not say which port it would use: $out"
if grep -q 'command not found' <<< "$out"; then
    fail "--screenshots executes prose from its generated-config heredoc: $out"
fi
grep -q -- '--fake-discord' <<< "$out" \
    || fail "--screenshots --dry-run does not show that the server is a fake: $out"
grep -q 'nothing was built, started, or photographed' <<< "$out" \
    || fail "--screenshots --dry-run does not say it did nothing: $out"
grep -qv ':8080' <<< "$out" || fail "--screenshots --dry-run mentions port 8080: $out"
ok "--screenshots --dry-run plans a throwaway fake server on 18091 and starts nothing"

# It must need NONE of the owner's configuration: this is the action you reach for on a machine
# with no deployment at all, and reading ~/.config/vibe-talk/env would hand it the real bot token
# for no reason. run_sh_isolated_config points XDG_CONFIG_HOME at an empty tree.
out="$(run_sh_isolated_config --screenshots --dry-run)"
grep -q '127.0.0.1:18091' <<< "$out" \
    || fail "--screenshots needs a configuration file it should never read: $out"
ok "--screenshots reads none of the owner's configuration"

rc=0; out="$(run_sh --theme dark)" || rc=$?
[ "$rc" -ne 0 ] || fail "--theme on its own exited 0"
grep -q -- '--theme only means something together with --screenshots' <<< "$out" \
    || fail "--theme on its own does not say what it needs: $out"
ok "--theme on its own refuses, naming the flag it belongs to"

rc=0; out="$(run_sh --screenshots --theme purple)" || rc=$?
[ "$rc" -ne 0 ] || fail "--theme with a nonsense value exited 0"
grep -q 'must be dark, light or both' <<< "$out" \
    || fail "--theme does not name the values it accepts: $out"
ok "--theme rejects a value it cannot honour, naming the valid ones"

# DARK IS THE DEFAULT, and that is the whole point rather than a preference. The owner's phone is
# dark; the first run of this harness captured nothing but light frames because that is the browser
# automation default, so every image reviewed a page he never sees. A silent revert to light would
# reproduce exactly that, and nothing else in this suite would notice.
out="$(run_sh --screenshots --dry-run)"
grep -q 'theme:     dark' <<< "$out" \
    || fail "--screenshots no longer defaults to the dark theme: $out"
grep -q -- '--theme dark' <<< "$out" \
    || fail "--screenshots --dry-run does not pass the theme through to the harness: $out"
ok "--screenshots defaults to dark and passes the theme through"

# The capture harness's OWN controls, run here for the same reason the smoke test's are: if they
# fail, a green screenshot run means nothing, because nothing would reject a blank picture.
rc=0; out="$(timeout 120 python3 "$SCRIPT_DIR/screenshots.py" --self-test 2>&1)" || rc=$?
printf '%s\n' "$out" >> "$ALL_OUTPUT"
[ "$rc" -eq 0 ] || fail "the screenshot harness's own controls FAILED: $out"
grep -q 'a flat WHITE frame is rejected' <<< "$out" \
    || fail "the screenshot controls do not include the blank-frame negative control: $out"
grep -q 'record_capture REFUSES to certify a blank frame it wrote' <<< "$out" \
    || fail "the screenshot controls do not check the gate on the path that SAVES files: $out"
grep -q "unreachable state's error names the state" <<< "$out" \
    || fail "the screenshot controls do not check that a missed state fails by name: $out"
grep -q 'no state still drives a selector the interface rework retired' <<< "$out" \
    || fail "the screenshot controls do not guard against stale post-rework selectors: $out"
grep -q 'dark is the default theme' <<< "$out" \
    || fail "the screenshot controls do not pin dark as the default theme: $out"
ok "the screenshot harness's own controls pass (blank, save-path and unreachable-state included)"

# A missing browser must say so BY NAME with the command that fixes it. Pointing the browser
# search path at an empty directory is the real failure a fresh machine hits.
rc=0
out="$(timeout 120 env PLAYWRIGHT_BROWSERS_PATH="$TMPDIR_TEST/no-browsers" \
        VIBE_TALK_WRITE_TOKEN=irrelevant-for-this-check \
        python3 "$SCRIPT_DIR/screenshots.py" --url "http://127.0.0.1:$ABSENT_PORT" \
        --out "$TMPDIR_TEST/shots" 2>&1)" || rc=$?
printf '%s\n' "$out" >> "$ALL_OUTPUT"
[ "$rc" -eq 30 ] || fail "a missing browser should exit 30 (playwright_missing), got $rc: $out"
grep -q 'playwright install chromium' <<< "$out" \
    || fail "the missing-browser failure does not carry the install command: $out"
# Non-vacuity. This run also points at a dead server; if the server were checked FIRST the run
# would report that instead and this control would be certifying a message it never saw.
grep -qv 'nothing answered at' <<< "$out" \
    || fail "the missing-browser check never reached the browser — it reported the server instead: $out"
ok "a missing browser fails by name, exit 30, with the install command"

# The ONE check in this file that needs a working browser. To report an unreachable server the
# harness must first get PAST its own browser precondition; without one it exits 30 for exactly
# the reason the previous check just certified, so this would be asserting the wrong failure.
#
# Probed rather than assumed. Every other check here runs on a machine with no playwright at all
# — which is the point, because this suite is what you are told to run before a deploy, and a
# suite that needs a 150MB browser download to say anything about --logs is a suite nobody runs.
BROWSER_PATH="$(python3 - <<'PROBE' 2>/dev/null || true
from playwright.sync_api import sync_playwright

with sync_playwright() as play:
    print(play.chromium.executable_path)
PROBE
)"
if [ -n "$BROWSER_PATH" ] && [ -x "$BROWSER_PATH" ]; then
    rc=0
    out="$(timeout 60 env VIBE_TALK_WRITE_TOKEN=irrelevant-for-this-check \
            python3 "$SCRIPT_DIR/screenshots.py" --url "http://127.0.0.1:$ABSENT_PORT" \
            --out "$TMPDIR_TEST/shots" 2>&1)" || rc=$?
    printf '%s\n' "$out" >> "$ALL_OUTPUT"
    [ "$rc" -eq 33 ] || fail "an unreachable server should exit 33 (server_unreachable), got $rc: $out"
    grep -q "127.0.0.1:$ABSENT_PORT" <<< "$out" \
        || fail "the unreachable-server failure does not name the URL it tried: $out"
    ok "the screenshot harness reports an unreachable server as its own failure, naming the URL"
else
    echo "SKIP no playwright chromium: the unreachable-server exit code was not checked" >&2
fi

rc=0
out="$(timeout 60 env -u VIBE_TALK_WRITE_TOKEN python3 "$SCRIPT_DIR/screenshots.py" \
        --url "http://127.0.0.1:$ABSENT_PORT" --out "$TMPDIR_TEST/shots" 2>&1)" || rc=$?
printf '%s\n' "$out" >> "$ALL_OUTPUT"
[ "$rc" -eq 2 ] || fail "a missing write token should be a usage error (exit 2), got $rc: $out"
grep -q 'VIBE_TALK_WRITE_TOKEN' <<< "$out" \
    || fail "the missing-token failure does not name the variable: $out"
ok "the screenshot harness names VIBE_TALK_WRITE_TOKEN when it is not set"

rc=0
out="$(timeout 60 env VIBE_TALK_WRITE_TOKEN=irrelevant-for-this-check \
        python3 "$SCRIPT_DIR/screenshots.py" --url "http://127.0.0.1:$ABSENT_PORT" \
        --out "$TMPDIR_TEST/shots" --only no-such-state 2>&1)" || rc=$?
printf '%s\n' "$out" >> "$ALL_OUTPUT"
[ "$rc" -ne 0 ] || fail "--only with an unknown state name exited 0: $out"
ok "the screenshot harness refuses an unknown state name"

# --- 10. already-running detection, by image and by port -------------------------------------
if command -v "$ENGINE" >/dev/null 2>&1; then
    BASE_IMAGE="$("$ENGINE" images --format '{{.Repository}}:{{.Tag}}' | grep -E '^docker.io/library/debian:' | head -1 || true)"
    # This suite's OWN throwaway images are excluded from the fallback: the write-probe one is
    # FROM scratch and has no shell, so building the detection image on top of it produces a
    # container that exits at once and is then, correctly, not detected as running — an unrelated
    # check failing for a reason that is not in it.
    [ -n "$BASE_IMAGE" ] || BASE_IMAGE="$("$ENGINE" images --format '{{.Repository}}:{{.Tag}}' | grep -v '<none>' | grep -v 'vibe-talk-selftest' | head -1 || true)"
    if [ -z "$BASE_IMAGE" ]; then
        echo "SKIP no local image to tag for the detection test" >&2
    else
        # Built rather than just tagged, so the image has a CMD of its own: run.sh launches an
        # image with no arguments, and a container that says nothing would let a log check pass
        # on empty output. Local base image only, --pull=never, so this needs no network.
        cat > "$TMPDIR_TEST/Containerfile.selftest" <<EOF
FROM $BASE_IMAGE
CMD ["sh", "-c", "trap 'exit 0' TERM; echo $LOG_MARKER; sleep 600 & wait"]
EOF
        "$ENGINE" build --pull=never -t "${TEST_IMAGE_NAME}:${TEST_IMAGE_TAG}" \
            -f "$TMPDIR_TEST/Containerfile.selftest" "$TMPDIR_TEST" >/dev/null 2>&1 \
            || fail "could not build the throwaway self-test image from $BASE_IMAGE"
        # Port matching has to anchor on ':<port>->', because the Ports column names BOTH sides
        # of the mapping. Every vibe-talk container renders '->8080/tcp', so an unanchored match
        # on the owner's real port 8080 would sweep up any unrelated container publishing to
        # 8080 — and under --shutdown this is what decides what gets STOPPED. The decoy maps host
        # 18096 to container port 18099: its Ports string contains 18099, but never ':18099->'.
        # (Mutation-tested: relaxing the anchor to a bare substring survived until this existed.)
        DECOY_CONTAINER="$("$ENGINE" run -d -p "127.0.0.1:18096:${TEST_PORT}" "$BASE_IMAGE" sleep 120)"
        sleep 1
        rc=0; out="$(run_sh --config "$GOOD_CONFIG" --dry-run --no-tunnel --tag decoy-probe)" || rc=$?
        [ "$rc" -eq 0 ] || fail "a container that mentions $TEST_PORT only on the container side was matched as running: $out"
        grep -q 'already running' <<< "$out" && fail "port matching is not anchored: the decoy was taken for a running instance"
        "$ENGINE" rm -f "$DECOY_CONTAINER" >/dev/null
        DECOY_CONTAINER=""
        ok "a container that publishes TO our port, rather than ON it, is not mistaken for ours"

        CREATED_CONTAINER="$("$ENGINE" run -d -p "127.0.0.1:${TEST_PORT}:8080" \
            "${TEST_IMAGE_NAME}:${TEST_IMAGE_TAG}" sleep 600)"
        sleep 1

        rc=0; out="$(run_sh --config "$GOOD_CONFIG" --dry-run --no-tunnel)" || rc=$?
        [ "$rc" -ne 0 ] || fail "already-running container was not detected"
        grep -q 'already running' <<< "$out" || fail "already-running message is not specific"
        grep -q -- '--shutdown' <<< "$out" || fail "already-running message does not tell the user to pass --shutdown"
        ok "an already-running container is detected and the message names --shutdown"

        out="$(run_sh --config "$GOOD_CONFIG" --status)"
        # podman prints the short (12-character) id; the run above returned the full one.
        grep -q "${CREATED_CONTAINER:0:12}" <<< "$out" || fail "--status did not list the running container"
        ok "--status lists the running container"

        # An image reference of the SAME LENGTH as the running one must not match it. This
        # is the regression test for a real false match: index() returns 0 when the substring
        # is absent, and the old code compared that 0 against length(image) - length(target),
        # which is also 0 for equal-length references. "vibe-talk-selftest:samelength12" is 31
        # characters, exactly like the running "localhost/vibe-talk-selftest:t0". The port is
        # moved off the running one too, so the image comparison is what is under test.
        rc=0; out="$(run_sh --config "$GOOD_CONFIG" --dry-run --no-tunnel \
                        --tag samelength12 --port 18098)" || rc=$?
        [ "$rc" -eq 0 ] || fail "an equal-length, unrelated image reference was matched as running: $out"
        grep -q 'already running' <<< "$out" && fail "equal-length image reference falsely matched the running container"
        grep -q 'Running' <<< "$out" || fail "equal-length-image run did not reach the run step"
        ok "an unrelated image of the same reference length is not mistaken for a running instance"

        # The owner's real container is on port 8080 with image vibe-talk:v0. Neither the
        # throwaway image name nor the throwaway port can match it.
        out="$(run_sh --config "$GOOD_CONFIG" --shutdown)"
        grep -q 'Not rebuilding or relaunching' <<< "$out" || fail "--shutdown did not say it stops and exits"
        sleep 1
        if "$ENGINE" ps --format '{{.ID}}' | grep -q "${CREATED_CONTAINER:0:12}"; then
            fail "--shutdown did not stop the container"
        fi
        ok "--shutdown stops the container and exits without rebuilding"

        out="$(run_sh --config "$GOOD_CONFIG" --shutdown)"
        grep -q 'Nothing to stop' <<< "$out" || fail "--shutdown on nothing running is not a clean no-op"
        ok "--shutdown is idempotent when nothing is running"

        # --shutdown must KEEP the stopped container. Under the old --rm launch it vanished, and
        # with it the access log of whatever had just gone wrong.
        "$ENGINE" ps -a --format '{{.ID}}' | grep -q "${CREATED_CONTAINER:0:12}" \
            || fail "the container disappeared when it was stopped: its logs are gone"
        out="$(run_sh --config "$GOOD_CONFIG" --status)"
        grep -q 'stopped (kept for their logs' <<< "$out" || fail "--status does not report the stopped container: $out"
        ok "a stopped container survives, and --status says it is being kept for its logs"

        # --- 12. --status reports the new operational facts ----------------------------------
        out="$(run_sh --config "$GOOD_CONFIG" --status)"
        grep -q "container name: $TEST_CONTAINER_NAME" <<< "$out" || fail "--status does not report the container name: $out"
        grep -q 'restart policy: on-failure:5' <<< "$out" || fail "--status does not report the restart policy: $out"
        grep -qi 'logs: *RETAINED' <<< "$out" || fail "--status does not say whether logs are retained: $out"
        grep -q -- '--logs' <<< "$out" || fail "--status does not say how to read the logs: $out"
        ok "--status reports the container name, the restart policy, and that logs are retained"

        # Reboot survival needs BOTH podman-restart.service and a policy of literally 'always',
        # so --status must report the unit's state rather than let the policy line imply it.
        grep -q 'podman-restart.service is ' <<< "$out" || fail "--status does not report reboot survival: $out"
        grep -q "literally 'always'" <<< "$out" || fail "--status does not say which policy podman-restart.service acts on: $out"
        ok "--status reports what would actually happen to the container after a reboot"

        # --- 13. the cut-over signal on an old-style (--rm) container -------------------------
        # This is what the owner's live container looks like right now: autoremove, no name, no
        # policy. --status must say so, because "it is up" hides that its logs are doomed.
        OLDSTYLE_CONTAINER="$("$ENGINE" run -d --rm -p "127.0.0.1:18096:8080" \
            "${TEST_IMAGE_NAME}:${TEST_IMAGE_TAG}")"
        sleep 1
        out="$(run_sh --config "$GOOD_CONFIG" --status)"
        grep -q 'autoremove=true' <<< "$out" || fail "--status does not report autoremove on an --rm container: $out"
        grep -q 'started the OLD way' <<< "$out" || fail "--status does not flag an --rm container for cut-over: $out"
        grep -q -- '--restart' <<< "$out" || fail "--status does not say how to cut over: $out"
        ok "--status flags an --rm container as losing its logs, and names the cut-over command"
        "$ENGINE" rm -f "$OLDSTYLE_CONTAINER" >/dev/null 2>&1 || true
        OLDSTYLE_CONTAINER=""

        # --- 14. a real launch, with only the image build stubbed out ------------------------
        # Everything else — detection, stopped-container sweep, the name, the restart policy,
        # the actual `podman run` — is real, and every assertion below reads real podman state
        # rather than run.sh's own account of itself. Only `build` is stubbed, because building
        # the real vibe-talk image here would cost minutes and prove nothing about run.sh.
        FAKE_ENGINE="$TMPDIR_TEST/fake-engine"
        cat > "$FAKE_ENGINE" <<EOF
#!/usr/bin/env bash
if [ "\${1:-}" = build ]; then echo "fake-engine: build stubbed (image is pre-built)"; exit 0; fi
exec "$ENGINE" "\$@"
EOF
        chmod +x "$FAKE_ENGINE"

        stopped_before="${CREATED_CONTAINER:0:12}"
        "$ENGINE" ps -a --format '{{.ID}}' | grep -q "$stopped_before" \
            || fail "test setup: expected the stopped container to still be present before the launch"

        rc=0
        out="$(ENGINE_FOR_RUN="$FAKE_ENGINE" run_sh --config "$GOOD_CONFIG" --no-tunnel)" || rc=$?
        [ "$rc" -eq 0 ] || fail "the launch failed: $out"
        grep -q "Started $TEST_CONTAINER_NAME" <<< "$out" || fail "the launch did not report starting the container: $out"
        grep -q -- '--follow' <<< "$out" || fail "the launch does not tell the operator how to follow the output: $out"
        ok "run.sh launches the container detached and says how to follow it"

        # The leftover stopped container was in the way of both the name and the port. It must
        # have been removed as part of the launch, not left to collide with it.
        grep -q 'Removing stopped container' <<< "$out" || fail "the launch did not report sweeping the stopped container: $out"
        "$ENGINE" ps -a --format '{{.ID}}' | grep -q "$stopped_before" \
            && fail "the stopped container was not removed, so the relaunch would collide with it"
        ok "a stopped container present before a launch is swept, not collided with"

        LAUNCHED_ID="$("$ENGINE" ps --filter "name=^${TEST_CONTAINER_NAME}$" --format '{{.ID}}')"
        [ -n "$LAUNCHED_ID" ] || fail "no running container carries the fixed name $TEST_CONTAINER_NAME"
        mode="$("$ENGINE" inspect --format '{{.HostConfig.RestartPolicy.Name}}:{{.HostConfig.RestartPolicy.MaximumRetryCount}}|{{.HostConfig.AutoRemove}}' "$TEST_CONTAINER_NAME")"
        [ "$mode" = "on-failure:5|false" ] \
            || fail "the launched container is not on-failure:5 with logs retained, it is: $mode"
        ok "the launched container has the fixed name, restart policy on-failure:5, and no --rm"

        # The driver has to be PINNED, and this is the check that says so. Inheriting the host
        # default is how the three --logs checks below came to fail on a rootless host whose
        # default is journald and whose journal the calling user cannot read: 'podman logs'
        # returned NOTHING, with no error, so a launcher that was working perfectly looked like a
        # server that had said nothing. Asserting the driver names that cause directly, instead of
        # leaving it to be rediscovered from three silent log checks.
        driver="$("$ENGINE" inspect --format '{{.HostConfig.LogConfig.Type}}' "$TEST_CONTAINER_NAME")"
        case "$driver" in
            k8s-file|json-file) ;;
            *) fail "the launched container did not pin a file log driver, it inherited: $driver" ;;
        esac
        ok "the launched container pins a file log driver rather than inheriting the host's"

        # --smoke-agent, DRY RUN ONLY. The suite must never hold a real conversation: that costs
        # the owner vendor minutes. What is checkable for free is everything up to the connection
        # — that it resolves the URL, the channel and the CONTAINER THAT IS ACTUALLY SERVING out
        # of the config and the running state, hands them to the smoke script, and bills nothing.
        out="$(run_sh --config "$GOOD_CONFIG" --smoke-agent --dry-run)"
        grep -q 'smoke-agent.py' <<< "$out" || fail "--smoke-agent does not reach the smoke script: $out"
        grep -q -- "--url http://127.0.0.1:$TEST_PORT" <<< "$out" \
            || fail "--smoke-agent did not pass the configured URL: $out"
        # The FIRST channel of the config's list, with its label and mode stripped off.
        grep -q -- '--channel 123456789012345678' <<< "$out" \
            || fail "--smoke-agent did not pass the first configured channel snowflake: $out"
        grep -q -- "--container $TEST_CONTAINER_NAME" <<< "$out" \
            || fail "--smoke-agent did not point the log check at the container that is serving: $out"
        grep -qi 'costs vendor minutes' <<< "$out" || fail "--smoke-agent does not warn about the cost: $out"
        grep -q 'nothing was connected to and nothing was billed' <<< "$out" \
            || fail "--smoke-agent --dry-run does not say it spent nothing: $out"
        ok "--smoke-agent resolves the URL, channel and serving container, and --dry-run bills nothing"

        # The credential must reach the smoke script through the ENVIRONMENT and never the command
        # line, which every process on the box can read. Checked here rather than only in the
        # end-of-suite leak scan, because this is the specific mechanism being relied on.
        grep -qF -- "$SENTINEL_WRITE_TOKEN" <<< "$out" \
            && fail "--smoke-agent put the write token on the command line: $out"
        grep -qF -- "$SENTINEL_READ_TOKEN" <<< "$out" \
            && fail "--smoke-agent put the read token on the command line: $out"
        ok "--smoke-agent passes tokens by environment, never on the command line"

        # --nonce is the mode that WRITES to the channel, so it must be visible in the plan.
        out="$(run_sh --config "$GOOD_CONFIG" --smoke-agent --nonce --dry-run)"
        grep -q -- '--nonce' <<< "$out" || fail "--nonce did not reach the smoke script: $out"
        grep -qi 'posts ONE unique token\|posts one unique token' <<< "$out" \
            || fail "--nonce does not say that it writes to the channel: $out"
        ok "--smoke-agent --nonce passes the flag on and says out loud that it writes to the channel"

        # And the DEFAULT plan must say that a failure writes to the channel too, via the
        # escalation. "Nothing is written" would be a promise the run does not keep.
        out="$(run_sh --config "$GOOD_CONFIG" --smoke-agent --dry-run)"
        grep -qi 'escalate' <<< "$out" \
            || fail "the default --smoke-agent plan does not mention the escalation: $out"
        ok "--smoke-agent's default plan says a failure escalates and holds a second conversation"

        sleep 1
        out="$(run_sh --config "$GOOD_CONFIG" --logs)"
        grep -q "$LOG_MARKER" <<< "$out" || fail "--logs did not show the running container's output: $out"
        ok "--logs dumps the running container's output"

        out="$(run_sh --config "$GOOD_CONFIG" --follow --dry-run)"
        grep -q "logs -f" <<< "$out" || fail "--follow does not stream (no 'logs -f'): $out"
        grep -qi 'Ctrl-C stops READING' <<< "$out" || fail "--follow does not say that interrupting it leaves the container up: $out"
        ok "--follow streams the output, and says that interrupting it does not stop the container"

        # The check above reads a PRINTED command, and a --follow that had lost its -f would
        # print exactly the same line while dumping and exiting — so it proves nothing about the
        # path that runs. (Mutation-tested: dropping -f from the exec survived until this check
        # existed.) Against a RUNNING container, --follow must not return on its own; timeout
        # ends it, and rc 124 is the evidence that it was still attached when time ran out.
        rc=0
        out="$(timeout 6 env -i PATH="$PATH" HOME="$HOME" USER="${USER:-}" \
                XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-}" VIBE_TALK_ENGINE="$ENGINE" \
                bash "$RUN_SH" --config "$GOOD_CONFIG" --follow 2>&1)" || rc=$?
        printf '%s\n' "$out" >> "$ALL_OUTPUT"
        [ "$rc" -eq 124 ] \
            || fail "--follow returned on its own (rc=$rc): it dumped the log and exited instead of following"
        grep -q "$LOG_MARKER" <<< "$out" || fail "--follow did not stream the container's output: $out"
        ok "--follow really stays attached to a running container instead of dumping and exiting"

        # THE point of dropping --rm: stop it, and the log is still readable afterwards.
        out="$(run_sh --config "$GOOD_CONFIG" --shutdown)"
        sleep 1
        "$ENGINE" ps -a --filter "name=^${TEST_CONTAINER_NAME}$" --format '{{.ID}}' | grep -q . \
            || fail "the container vanished when stopped: its logs went with it"
        out="$(run_sh --config "$GOOD_CONFIG" --logs)"
        grep -q "$LOG_MARKER" <<< "$out" || fail "--logs on a STOPPED container lost the output: $out"
        ok "logs survive stopping the container, which is what --rm used to destroy"

        # Relaunch over the stopped one: the fixed name must not collide.
        rc=0
        out="$(ENGINE_FOR_RUN="$FAKE_ENGINE" run_sh --config "$GOOD_CONFIG" --no-tunnel)" || rc=$?
        [ "$rc" -eq 0 ] || fail "relaunch over a stopped same-named container failed: $out"
        grep -qi 'in use\|already in use\|name is already' <<< "$out" && fail "relaunch hit a name collision: $out"
        RELAUNCHED_ID="$("$ENGINE" ps --filter "name=^${TEST_CONTAINER_NAME}$" --format '{{.ID}}')"
        [ -n "$RELAUNCHED_ID" ] || fail "nothing is running under $TEST_CONTAINER_NAME after the relaunch"
        [ "$RELAUNCHED_ID" != "$LAUNCHED_ID" ] || fail "the relaunch did not actually replace the container"
        [ "$("$ENGINE" ps -a --filter "name=^${TEST_CONTAINER_NAME}$" --format '{{.ID}}' | wc -l)" -eq 1 ] \
            || fail "more than one container now carries the name $TEST_CONTAINER_NAME"
        ok "relaunching over a stopped, same-named container replaces it cleanly"

        # A container can hold the fixed name while matching NEITHER the image nor the port —
        # someone started one by hand, or the tag moved. Detection is image-or-port by design and
        # will not see it, so without the name guard `run --name` fails with an engine-level
        # error that mentions nothing about this script.
        run_sh --config "$GOOD_CONFIG" --shutdown >/dev/null
        "$ENGINE" rm -f "$TEST_CONTAINER_NAME" >/dev/null 2>&1 || true
        FOREIGN="$("$ENGINE" create --name "$TEST_CONTAINER_NAME" -p "127.0.0.1:18095:8080" "$BASE_IMAGE" sleep 5)"
        rc=0
        out="$(ENGINE_FOR_RUN="$FAKE_ENGINE" run_sh --config "$GOOD_CONFIG" --no-tunnel)" || rc=$?
        [ "$rc" -eq 0 ] || fail "a stale container holding the fixed name blocked the launch: $out"
        grep -q 'holds the name' <<< "$out" || fail "the launch did not report clearing the name: $out"
        "$ENGINE" ps -a --format '{{.ID}}' | grep -q "${FOREIGN:0:12}" \
            && fail "the stale name-holder was not removed, so the name is still taken"
        "$ENGINE" ps --filter "name=^${TEST_CONTAINER_NAME}$" --format '{{.Image}}' | grep -q "$TEST_IMAGE_NAME" \
            || fail "the relaunched container is not ours"
        ok "a stale container holding the fixed name is cleared, not collided with"

        run_sh --config "$GOOD_CONFIG" --shutdown >/dev/null || true
        "$ENGINE" rm -f "$TEST_CONTAINER_NAME" >/dev/null 2>&1 || true
    fi
else
    echo "SKIP $ENGINE not available: container-detection checks not run" >&2
fi

# --- 11. no credential ever appears in run.sh's output ----------------------------------------
# Every invocation above appended its combined stdout+stderr to $ALL_OUTPUT, so this scans the
# whole suite's output in one pass — including the new --status, --logs, --follow and
# --tunnel-status paths, which each print a block of text that did not exist before.
leaks_in() {  # leaks_in FILE -> prints the sentinels found
    local file="$1" secret
    for secret in "$SENTINEL_BOT_TOKEN" "$SENTINEL_READ_TOKEN" "$SENTINEL_WRITE_TOKEN" "$SENTINEL_EL_KEY"; do
        if grep -qF -- "$secret" "$file"; then printf '%s\n' "${secret%%-*}"; fi
    done
}

found_leaks="$(leaks_in "$ALL_OUTPUT")"
[ -z "$found_leaks" ] || fail "credential value(s) appeared in run.sh output: $found_leaks"

# Vacuity guard. "No credential found" is worthless if the new actions' output never reached the
# file being scanned, so require each new output path to have actually contributed to it.
expected_markers=("Tunnel status:" "hostname:")
if [ -n "${LAUNCHED_ID:-}" ]; then
    expected_markers+=("container name: $TEST_CONTAINER_NAME" "Started $TEST_CONTAINER_NAME" "$LOG_MARKER")
else
    echo "SKIP the launch-path output could not be included in the leak scan ($ENGINE unavailable)" >&2
fi
for marker in "${expected_markers[@]}"; do
    grep -qF -- "$marker" "$ALL_OUTPUT" \
        || fail "the leak scan never saw output from a new action (missing marker: '$marker')"
done

# Positive control, run through the SAME scanner over a copy of the SAME output: if a credential
# had been printed anywhere in it, the check above would have said so.
cp "$ALL_OUTPUT" "$TMPDIR_TEST/control.txt"
printf '%s\n' "$SENTINEL_READ_TOKEN" >> "$TMPDIR_TEST/control.txt"
[ -n "$(leaks_in "$TMPDIR_TEST/control.txt")" ] \
    || fail "the leak check itself is broken: it cannot find a value planted in the very file it scans"
ok "no credential value appears anywhere in run.sh's output (across $(wc -l < "$ALL_OUTPUT") lines, ${#expected_markers[@]} new output paths included)"

echo ""
echo "all $PASS checks passed"
