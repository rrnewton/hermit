# shellcheck shell=bash
#
# Collect ALL missing demo prerequisites and report them in one shot.
#
# WHY. Every demo discovered its prerequisites serially: the first missing one
# called `exit 1`, you installed it, re-ran, and hit the next. That is the worst
# shape for tire-kicking, because each error looks like THE blocker -- you
# cannot tell whether you are one step or five from a working demo, and each
# step costs a full re-run of everything that already succeeded.
#
# These helpers RECORD a missing prerequisite instead of exiting, so
# `preflight_report` can name the complete set with per-item remediation.
#
# A prerequisite is not only a package. A blocked download is just as much a
# missing prerequisite as an absent binary -- demo 05 needs a kernel image that
# the egress allowlist can refuse -- so `preflight_require_url` is a first-class
# check alongside commands and files.
#
# Usage:
#   source "$DEMO_DIR/lib/preflight.sh"
#   preflight_require_command qemu-system-x86_64 "dnf install qemu-system-x86"
#   preflight_require_file "$KERNEL" "set KERNEL_IMAGE=/path/to/bzImage"
#   preflight_require_url "$KERNEL_URL" "needs egress to kernel.org; set KERNEL_IMAGE to a local file"
#   preflight_report            # prints every miss, exits 1 if any

_PREFLIGHT_MISSING=()

# Record rather than exit. That single choice is what turns serial discovery
# into a complete set.
_preflight_record() {
    _PREFLIGHT_MISSING+=("$1"$'\t'"$2"$'\t'"$3")
}

preflight_require_command() {
    local name=$1 remedy=${2:-"install $1"}
    command -v "$name" >/dev/null 2>&1 || _preflight_record "command" "$name" "$remedy"
}

preflight_require_file() {
    local path=$1 remedy=${2:-"create or point the demo at $1"}
    [ -f "$path" ] || _preflight_record "file" "$path" "$remedy"
}

preflight_require_executable() {
    local path=$1 remedy=${2:-"build or install $1"}
    [ -x "$path" ] || _preflight_record "executable" "$path" "$remedy"
}

# A NON-PACKAGE prerequisite: a fetch the environment may refuse. Checked with a
# HEAD request so it costs nothing, and routed through with-proxy when present
# because that is how this environment reaches the outside world at all.
# Skipped (not failed) when curl itself is missing -- curl will already have
# been reported as its own missing command, and reporting a download failure
# caused by a missing downloader would be a misleading second symptom of one
# cause.
preflight_require_url() {
    local url=$1 remedy=${2:-"make $1 reachable, or set the corresponding *_IMAGE/*_PATH override to a local copy"}
    command -v curl >/dev/null 2>&1 || return 0
    local curl_cmd=(curl --fail --location --silent --show-error --head --max-time 20)
    if command -v with-proxy >/dev/null 2>&1; then
        curl_cmd=(with-proxy "${curl_cmd[@]}")
    fi
    "${curl_cmd[@]}" "$url" >/dev/null 2>&1 ||
        _preflight_record "download" "$url" "$remedy"
}

preflight_missing_count() { printf '%s\n' "${#_PREFLIGHT_MISSING[@]}"; }

# Print the COMPLETE set, then exit non-zero. Nothing before this point exits on
# a missing prerequisite, which is the whole point.
preflight_report() {
    local label=${1:-demo}
    if [ "${#_PREFLIGHT_MISSING[@]}" -eq 0 ]; then
        return 0
    fi
    {
        printf '\n=== %s: %d missing prerequisite(s) ===\n' \
            "$label" "${#_PREFLIGHT_MISSING[@]}"
        printf 'All of the following are missing. This is the COMPLETE set, not the first one:\n\n'
        local entry kind what remedy
        for entry in "${_PREFLIGHT_MISSING[@]}"; do
            IFS=$'\t' read -r kind what remedy <<<"$entry"
            printf '  [%s] %s\n      -> %s\n' "$kind" "$what" "$remedy"
        done
        printf '\nFix all of them, then re-run. Nothing above was attempted.\n'
    } >&2
    exit 1
}
