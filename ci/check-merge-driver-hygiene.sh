#!/usr/bin/env bash
# Refuse a repository whose committed content can be rewritten by a merge driver
# that nobody can review.
#
# THE FAILURE THIS EXISTS FOR, observed 2026-08-08 on hermit#1864: a conflict-free
# `git rebase` silently re-serialized ci/dag/portable.json end to end -- the delta
# went from 99,021 bytes to 155,143, keys reordered, hunk `@@ -1,492 +1,947 @@` --
# with NO conflict, NO warning, and a clean `git status`. The cause was
# `$GIT_DIR/info/attributes`, an UNTRACKED, machine-local file that bound
# ci/dag/*.json and the e2e manifests to custom merge drivers that re-serialize.
# The default rebase backend runs a 3-way merge per file, so the driver fires even
# when the patch would have applied cleanly.
#
# Why that is worse than a formatting nit: a reviewer approves an exact head, and
# a routine rebase afterwards rewrites bytes they never saw, on one machine, with
# nothing in version control to show it happened.
#
# THE PREDICATE. A custom `merge=<driver>` attribute may bind a path only when it
# is declared by a TRACKED `.gitattributes`. Attributes from untracked sources
# ($GIT_DIR/info/attributes, core.attributesFile, the per-user and system files,
# and untracked in-tree .gitattributes) are invisible to review and to CI, so a
# `merge=` line in one is refused. A bound driver whose command is not a tracked
# file is also refused: an unreviewable program must not rewrite committed content.
#
# Built-in merge attribute values (text/binary/union) select git's own behaviour
# rather than an external program, so they are allowed from any source.
#
# Exit status:
#   0  no unreviewable merge-driver binding
#   1  at least one refused binding (details on stdout)
#   2  usage / internal error

set -uo pipefail

root=$(git rev-parse --show-toplevel 2>/dev/null) || {
    echo "merge-driver hygiene: not inside a git work tree" >&2
    exit 2
}
git_dir=$(git rev-parse --git-common-dir 2>/dev/null) || exit 2
case "$git_dir" in
    /*) ;;
    *) git_dir="$root/$git_dir" ;;
esac

# Built-in attribute values that do not name an external driver program.
builtin_value() {
    case "$1" in
        text | binary | union) return 0 ;;
        *) return 1 ;;
    esac
}

problems=0

report() {
    problems=$((problems + 1))
    printf 'REFUSED: %s\n' "$1"
}

# --- 1. Untracked attribute sources that declare a custom merge driver --------
#
# Every source git consults, in its own precedence order, minus tracked
# .gitattributes (which are reviewable and therefore allowed).
sources=()
sources+=("$git_dir/info/attributes")
if user_attrs=$(git config --get core.attributesFile 2>/dev/null) && [[ -n $user_attrs ]]; then
    sources+=("${user_attrs/#\~/$HOME}")
else
    sources+=("${XDG_CONFIG_HOME:-$HOME/.config}/git/attributes")
fi
sources+=("/etc/gitattributes")

# In-tree .gitattributes files that are not tracked are just as invisible.
while IFS= read -r untracked; do
    [[ -n $untracked ]] && sources+=("$root/$untracked")
done < <(git ls-files --others --exclude-standard -- '*.gitattributes' '.gitattributes' 2>/dev/null)

declare -A bound_drivers=()

for src in "${sources[@]}"; do
    [[ -f $src ]] || continue
    lineno=0
    while IFS= read -r line || [[ -n $line ]]; do
        lineno=$((lineno + 1))
        # Strip comments and blank lines.
        [[ $line =~ ^[[:space:]]*(#|$) ]] && continue
        # A merge attribute is `merge=NAME` anywhere in the attribute list.
        [[ $line =~ merge=([^[:space:]]+) ]] || continue
        driver=${BASH_REMATCH[1]}
        builtin_value "$driver" && continue
        pattern=${line%%[[:space:]]*}
        report "$src:$lineno binds '$pattern' to custom merge driver '$driver'.
         That file is NOT tracked, so the binding cannot be reviewed and does not
         exist on other machines. Any 3-way merge -- including a conflict-free
         rebase -- runs that driver and may rewrite the file with no warning.
         Fix: delete the line, or move the binding into a tracked .gitattributes."
        bound_drivers[$driver]=1
    done < "$src"
done

# --- 2. Drivers bound by a TRACKED .gitattributes must be tracked programs ----
while IFS= read -r attrs_file; do
    [[ -n $attrs_file ]] || continue
    lineno=0
    while IFS= read -r line || [[ -n $line ]]; do
        lineno=$((lineno + 1))
        [[ $line =~ ^[[:space:]]*(#|$) ]] && continue
        [[ $line =~ merge=([^[:space:]]+) ]] || continue
        driver=${BASH_REMATCH[1]}
        builtin_value "$driver" && continue
        bound_drivers[$driver]=1
    done < "$root/$attrs_file"
done < <(git ls-files -- '*.gitattributes' '.gitattributes' 2>/dev/null)

for driver in "${!bound_drivers[@]}"; do
    command=$(git config --get "merge.$driver.driver" 2>/dev/null) || command=""
    if [[ -z $command ]]; then
        # A bound-but-undefined driver is not a rewrite risk (git falls back to
        # the normal 3-way merge), but it means the two halves disagree.
        report "merge driver '$driver' is bound by an attributes file but has no
         merge.$driver.driver command in this repository's config. The binding and
         the program are configured in different places, only one of which you can
         see. Fix: define it in a tracked file, or remove the binding."
        continue
    fi
    # Locate the program: the first token that is not a %-placeholder or an
    # option. A token WITHOUT a slash is resolved by git through PATH, so it
    # names a machine-local executable that no reviewer of this repository can
    # see -- exactly the thing this check exists to refuse. Selecting only
    # slash-bearing tokens used to leave `program` empty for that case and the
    # `-n $program` guard below then FAILED OPEN.
    program=""
    for token in $command; do
        case "$token" in
            %*) continue ;;
            -*) continue ;;
            *) program=$token; break ;;
        esac
    done
    if [[ -z $program ]]; then
        report "merge driver '$driver' has a merge.$driver.driver command
         ('$command') with no program token at all. A driver whose program cannot
         be identified cannot be reviewed.
         Fix: define the driver as a repo-relative path, e.g. ./ci/my-driver.sh."
        continue
    fi
    case "$program" in
        */*) ;;
        *)
            report "merge driver '$driver' runs '$program', a BARE COMMAND NAME.
         git resolves it through PATH, so which executable rewrites your committed
         content depends on the machine, not on anything a reviewer can see -- and
         nothing in this repository pins it.
         Fix: point the driver at a tracked repo-relative path, e.g. ./ci/$program."
            continue
            ;;
    esac
    if git ls-files --error-unmatch "$program" >/dev/null 2>&1; then
        continue
    fi
    rel=${program#"$root"/}
    if [[ $rel != "$program" ]] && git ls-files --error-unmatch "$rel" >/dev/null 2>&1; then
        continue
    fi
    report "merge driver '$driver' runs '$program', which is NOT tracked by this
         repository. An unreviewable program must not be allowed to rewrite
         committed content during a merge or rebase.
         Fix: track the program, or remove the driver."
done

if ((problems > 0)); then
    cat <<'EOF'

Diagnose a specific path with:  git check-attr merge -- <path>
Verify a rebase preserved your delta with:  ci/check-rebase-preserved-delta.sh
A conflict-free rebase can still rewrite a file; `git status` stays clean.
EOF
    echo
    echo "check-merge-driver-hygiene.sh: FAIL — $problems unreviewable merge-driver binding(s)"
    exit 1
fi

echo "check-merge-driver-hygiene.sh: OK — no custom merge driver is bound from an unreviewable source"
exit 0
