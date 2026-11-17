#!/usr/bin/env python3
"""Read a Buck E2E cell subset (ci/buck-e2e/subsets/NAME.json) against the plan.

usage: subset.py SUBSET_JSON PLAN_JSON plan OUT     write the plan narrowed to the subset
       subset.py SUBSET_JSON PLAN_JSON buck-config  print the -c flags that define it in Buck
       subset.py SUBSET_JSON PLAN_JSON categories   print its categories, one per line
       subset.py SUBSET_JSON PLAN_JSON lanes        print its lanes, one per line
       subset.py SUBSET_JSON PLAN_JSON excluded-backends
                                                    print the plan's backends it leaves out

A subset names lanes, backends and categories; its cells are the plan cells in all three.
Every name must occur in the plan (a typo would otherwise define an empty or smaller
subset silently), and the subset must hold at least one cell. The Buck flags make the
open-source stand-in of fb-only/e2e_cells.bzl define exactly these cells, and the import
twins of scripts/validate.rs --buck-s0 select the same cells with --lane, --category and
--exclude-backend, so ingest, Buck and the judge agree on one set.
"""
import json
import sys


def refuse(message):
    print(f"subset.py: {message}", file=sys.stderr)
    sys.exit(2)


def load(subset_path, plan_path):
    try:
        with open(subset_path) as f:
            subset = json.load(f)
        with open(plan_path) as f:
            plan = json.load(f)
    except (OSError, ValueError) as error:
        refuse(f"cannot read {subset_path} or {plan_path}: {error}")
    for key in ("lanes", "backends", "categories"):
        values = subset.get(key)
        if not isinstance(values, list) or not values or not all(isinstance(v, str) and v for v in values):
            refuse(f"{subset_path}: {key} must be a non-empty list of names")
        known = {cell[key[:-1] if key != "categories" else "category"] for cell in plan["cells"]}
        unknown = sorted(set(values) - known)
        if unknown:
            refuse(f"{subset_path}: {key} {unknown} do not occur in {plan_path}")
    cells = [
        cell
        for cell in plan["cells"]
        if cell["lane"] in subset["lanes"]
        and cell["backend"] in subset["backends"]
        and cell["category"] in subset["categories"]
    ]
    if not cells:
        refuse(f"{subset_path} selects no cell of {plan_path}")
    return subset, plan, cells


def main(argv):
    if len(argv) < 4:
        refuse(__doc__.strip().splitlines()[2])
    subset, plan, cells = load(argv[1], argv[2])
    action = argv[3]
    if action == "plan" and len(argv) == 5:
        narrowed = dict(plan, cells=cells)
        with open(argv[4], "w") as f:
            json.dump(narrowed, f)
        print(len(cells))
    elif action == "buck-config" and len(argv) == 4:
        for key in ("lanes", "backends", "categories"):
            print(f"-c\nhermit_e2e.subset_{key}={','.join(sorted(subset[key]))}")
    elif action == "categories" and len(argv) == 4:
        print("\n".join(sorted(subset["categories"])))
    elif action == "lanes" and len(argv) == 4:
        print("\n".join(sorted(subset["lanes"])))
    elif action == "excluded-backends" and len(argv) == 4:
        backends = {cell["backend"] for cell in plan["cells"]}
        print("\n".join(sorted(backends - set(subset["backends"]))))
    else:
        refuse(f"unknown action or arguments: {argv[3:]}")


if __name__ == "__main__":
    main(sys.argv)
