#!/usr/bin/env python3
import re, sys

path = sys.argv[1]
deltas = {
    "test.hermit_unit": 1,
    "test.cli": 1,
    "test.cli_on_host": 1,
    "test.hermit_unit_on_host": 1,
}
text = open(path).read()
pin_re = re.compile(r'\("([^"]+)",\s*(\d+)\)')

def resolve_block(m):
    head, branch = m.group(1), m.group(2)
    hp = pin_re.search(head)
    bp = pin_re.search(branch)
    if not hp or not bp:
        raise SystemExit(f"conflict block without pin pair:\n{m.group(0)[:400]}")
    key, hv = hp.group(1), int(hp.group(2))
    if bp.group(1) != key:
        raise SystemExit(f"pin key mismatch {key} vs {bp.group(1)}")
    if key not in deltas:
        raise SystemExit(f"unexpected pin key {key}")
    nv = hv + deltas[key]
    # Branch comment lines (// drain/4709: ...) with arithmetic updated.
    comments = []
    for line in branch.splitlines():
        s = line.strip()
        if s.startswith("// drain/4709:") or s.startswith("// And "):
            # Replace "OLD + d = RES" arithmetic with new base/result.
            s2 = re.sub(r"\b\d+\s*\+\s*(\d+)\s*=\s*\d+", lambda mm: f"{hv} + {mm.group(1)} = {nv}", s)
            comments.append("    " + s2)
    head_lines = [l for l in head.splitlines() if pin_re.search(l) is None]
    out = head_lines + comments + [f'    ("{key}", {nv}),']
    return "\n".join(out) + "\n"

new = re.sub(r"<<<<<<< HEAD\n(.*?)\n=======\n(.*?)\n>>>>>>> [^\n]*\n", resolve_block, text, flags=re.S)
if "<<<<<<<" in new or "=======" in new:
    raise SystemExit("unresolved markers remain")
open(path, "w").write(new)
print("resolved")
