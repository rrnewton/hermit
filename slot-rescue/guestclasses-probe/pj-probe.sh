set -euo pipefail
W=$(mktemp -d); trap 'rm -rf "$W"' EXIT
{ printf '%s\n' 'all: a.out b.out c.out d.out'
  for t in a b c d; do printf '%s.out:\n\t@printf "%s:%%s\\n" $$(expr 6 \\* 7) > %s.out\n' $t $t $t; done
} > "$W/Makefile"
make --no-print-directory -s -C "$W" -j4
cat "$W"/a.out "$W"/b.out "$W"/c.out "$W"/d.out | sort | tr '\n' ' '; echo
