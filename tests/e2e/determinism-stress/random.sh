#!/usr/bin/env bash

set -euo pipefail
# shellcheck source=tests/e2e/determinism-stress/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

guest=$(compile_c tests/c/random_sources.c random-sources)
show_native_variation "getrandom plus /dev/random and /dev/urandom" "$guest"
verify_guest "random sources" "$guest"

python=${PYTHON:-/usr/bin/python3}
[[ -x $python ]] || fail "Python not found: $python"
python_program='import os, random, secrets; print(random.getrandbits(128)); print(random.SystemRandom().getrandbits(128)); print(secrets.token_hex(16)); print(os.urandom(16).hex())'
show_native_variation "Python PRNGs" "$python" -c "$python_program"
verify_guest "Python PRNGs" "$python" -c "$python_program"

stress_success "random number generation"
