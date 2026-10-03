# shellcheck shell=bash
#
# Let a program that a demo runs under `hermit run` reach its own executable
# and the files named on its command line when they are under the host's /tmp.
#
# `hermit run` gives the program a private, empty /tmp, so the program cannot
# see anything under the host's /tmp, and Hermit refuses to start a program
# that is there. `hermit run --tmp=/tmp` would show the program everything in
# the host's /tmp, which would make all of it an input to the run. Instead,
# these functions pass `--bind` for exactly what the program needs: the
# directory that holds the program, and each other file by itself. Hermit
# mounts each one at the same path inside the private /tmp. Nothing is added
# for a path outside /tmp, so the demo's command line for a checkout outside
# /tmp is the same as without these functions.
#
# The program is bound through its directory because Hermit cannot start a
# program whose path is itself a --bind target. Hermit looks the program up on
# the host as the bind's source joined with the rest of the program's path
# after the bind's target (mapped_path in hermit-cli/src/bin/hermit/run.rs).
# When the program is the target, the rest is empty, the join leaves the path
# with a trailing slash, and a file path with a trailing slash is not found
# (ENOTDIR), so Hermit reports that the program does not exist
# (https://github.com/rrnewton/hermit/issues/3626). Binding the directory also
# shows the program the directory's other entries, which in the
# demos are the program alone or files tracked in the repository. A symbolic
# link inside a bound directory is resolved in the program's view, where the
# rest of the host's /tmp is hidden.
#
# hermit_tmp_check_paths HINT PROGRAM [FILE...]
#   Return 0 if PROGRAM and every FILE are outside /tmp or can be shown to the
#   program. Otherwise print an error about the first that cannot, followed by
#   HINT, on standard error and return 1.
#
#   A path that begins with /tmp/ can be shown by binding it unless it contains
#   `:` (which --bind reads as SOURCE:TARGET) or a `..` component (Hermit puts
#   the bound path at the same relative path inside the private /tmp, and `..`
#   would leave it), or it names /tmp itself. For PROGRAM, its directory is
#   bound, so PROGRAM must not be directly in /tmp (binding /tmp would show the
#   program all of it), and its directory must not be a symbolic link (Hermit
#   would mount it as a file).
#
#   A path that reaches /tmp without beginning with /tmp/ is refused when it is
#   absolute (through a symbolic link), when it is PROGRAM (Hermit starts a
#   program by the absolute path it resolves on the host, which then begins
#   with /tmp/ and is hidden), or when it is a relative FILE that reaches /tmp
#   through a `..` component or a symbolic link. Any other relative FILE is
#   reached through the working directory, which the program inherits as it
#   is, so it is left unchanged.
#
# hermit_tmp_bind_args ARRAY PROGRAM [FILE...]
#   Set the caller's array named ARRAY to `--bind DIR` for PROGRAM's directory
#   when it can be bound, followed by `--bind FILE` for each FILE that can be
#   bound and is not inside that directory, and to nothing else. It never
#   fails; call hermit_tmp_check_paths on the same paths first, so that a path
#   that cannot be shown stops the demo instead of being left out.
#
# hermit_tmp_is_under PATH
#   Return 0 if PATH begins with /tmp/ or resolves to /tmp or a path under it.

# Classify the path $2, which is the program when $1 is "program" and another
# file when $1 is "file". Set _HERMIT_TMP_CLASS to "bind", "unchanged", or
# "refuse"; for "bind", _HERMIT_TMP_BIND to the path to pass to --bind and
# _HERMIT_TMP_NORMAL to that path without `.` components or repeated or
# trailing slashes; for "refuse", _HERMIT_TMP_PROBLEM to the reason.
_hermit_tmp_classify() {
  local kind="$1" path="$2" resolved logical
  _HERMIT_TMP_CLASS=unchanged
  _HERMIT_TMP_BIND=
  _HERMIT_TMP_NORMAL=
  _HERMIT_TMP_PROBLEM=
  case "$path" in
    /tmp/*)
      _HERMIT_TMP_CLASS=refuse
      case "$path" in
        *:*)
          _HERMIT_TMP_PROBLEM="it contains ':', which --bind reads as SOURCE:TARGET"
          return
          ;;
      esac
      case "/$path/" in
        */../*)
          _HERMIT_TMP_PROBLEM="it contains a '..' component"
          return
          ;;
      esac
      if [ "$kind" = program ]; then
        _HERMIT_TMP_BIND=$(dirname -- "$path")
      else
        _HERMIT_TMP_BIND=$path
      fi
      # Remove `.` components and repeated or trailing slashes, without
      # following symbolic links, to catch a spelling of /tmp itself.
      if ! _HERMIT_TMP_NORMAL=$(realpath -m -s -- "$_HERMIT_TMP_BIND" 2>/dev/null); then
        _HERMIT_TMP_PROBLEM="realpath could not normalize $_HERMIT_TMP_BIND"
        return
      fi
      if [ "$_HERMIT_TMP_NORMAL" = /tmp ]; then
        if [ "$kind" = program ]; then
          _HERMIT_TMP_PROBLEM="it is directly in /tmp, and binding its directory would show the program all of /tmp"
        else
          _HERMIT_TMP_PROBLEM="it names /tmp itself, not a file below it"
        fi
        return
      fi
      if [ "$kind" = program ] && [ -L "$_HERMIT_TMP_BIND" ]; then
        _HERMIT_TMP_PROBLEM="its directory, $_HERMIT_TMP_BIND, is a symbolic link, which Hermit would mount as a file"
        return
      fi
      _HERMIT_TMP_CLASS=bind
      ;;
    *)
      # realpath -m resolves the symbolic links and `..` in the part of the
      # path that exists and does not require the rest to exist. If it fails
      # (a symbolic-link loop, an unreadable directory), the path is passed
      # on unchanged, as it was before these functions existed.
      resolved=$(realpath -m -- "$path" 2>/dev/null) || return 0
      case "$resolved/" in
        /tmp/*) ;;
        *) return 0 ;;
      esac
      _HERMIT_TMP_CLASS=refuse
      case "$path" in
        /*)
          _HERMIT_TMP_PROBLEM="it resolves to $resolved, under /tmp, but it does not begin with /tmp/"
          return
          ;;
      esac
      if [ "$kind" = program ]; then
        _HERMIT_TMP_PROBLEM="Hermit starts a program by the absolute path it resolves on the host, $resolved, which is under /tmp"
        return
      fi
      case "/$path/" in
        */../*)
          _HERMIT_TMP_PROBLEM="it resolves to $resolved, under /tmp, through a '..' component"
          return
          ;;
      esac
      # The same path with the working directory's symbolic links resolved
      # and none of its own: it differs from $resolved only when one of the
      # path's own components is a symbolic link.
      logical=$(realpath -m -s -- "$(pwd -P)/$path" 2>/dev/null) || logical=
      if [ "$logical" != "$resolved" ]; then
        _HERMIT_TMP_PROBLEM="it resolves to $resolved, under /tmp, through a symbolic link"
        return
      fi
      _HERMIT_TMP_CLASS=unchanged
      ;;
  esac
}

hermit_tmp_check_paths() {
  local hint="$1" kind=program path
  shift
  for path in "$@"; do
    _hermit_tmp_classify "$kind" "$path"
    kind="file"
    if [ "$_HERMIT_TMP_CLASS" = refuse ]; then
      printf 'error: %s is under /tmp. Hermit gives the program it runs a private /tmp, and the demo cannot make this path visible there: %s. %s\n' \
        "$path" "$_HERMIT_TMP_PROBLEM" "$hint" >&2
      return 1
    fi
  done
  return 0
}

hermit_tmp_bind_args() {
  local -n _hermit_tmp_binds="$1"
  local path program_dir=
  shift
  _hermit_tmp_binds=()
  [ "$#" -gt 0 ] || return 0
  _hermit_tmp_classify program "$1"
  shift
  if [ "$_HERMIT_TMP_CLASS" = bind ]; then
    _hermit_tmp_binds+=(--bind "$_HERMIT_TMP_BIND")
    program_dir=$_HERMIT_TMP_NORMAL
  fi
  for path in "$@"; do
    _hermit_tmp_classify file "$path"
    [ "$_HERMIT_TMP_CLASS" = bind ] || continue
    # The program's directory already shows a file inside it.
    if [ -n "$program_dir" ]; then
      case "$_HERMIT_TMP_NORMAL/" in
        "$program_dir"/*) continue ;;
      esac
    fi
    _hermit_tmp_binds+=(--bind "$_HERMIT_TMP_BIND")
  done
  return 0
}

hermit_tmp_is_under() {
  local resolved
  case "$1" in
    /tmp/*) return 0 ;;
  esac
  resolved=$(realpath -m -- "$1" 2>/dev/null) || return 1
  case "$resolved/" in
    /tmp/*) return 0 ;;
  esac
  return 1
}
