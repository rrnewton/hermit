#!/usr/bin/env python3
"""Assemble the new image.provenance.json from two collect.py outputs."""
import hashlib, json, subprocess, sys
from pathlib import Path

wt = Path("/home/newton/work/dev-hermit/worktrees/slots/museops-20261028/hermetic-gtest-wt")
old = json.loads((wt / "ci/hermetic/image.provenance.json").read_text())
a = json.loads(Path(sys.argv[1]).read_text())
b = json.loads(Path(sys.argv[2]).read_text())
assert a["archive_sha256"] == b["archive_sha256"], "archives differ"
assert a["loaded"] == b["loaded"], "loaded digests differ"
assert a["output_logical"] == b["output_logical"]

def artifact(c):
    return {
        "archive_sha256": c["archive_sha256"],
        "archive_size_bytes": c["archive_size_bytes"],
        "loaded_reference": "localhost/hermit-hermetic-validate@" + c["loaded"]["loaded_digest"],
        "manifest_media_type": "application/vnd.docker.distribution.manifest.v2+json",
        "manifest_digest": c["loaded"]["loaded_digest"],
        "manifest_size_bytes": c["manifest_size_bytes"],
        "config_digest": c["config_digest"],
        "config_size_bytes": c["config_size_bytes"],
        "layers": [{"digest": l["sha256"], "size_bytes": l["size"]} for l in c["layers"]],
        "diff_ids": c["diff_ids"],
    }

def run(c):
    u = c["unit"]
    return {
        "run_id": c["identities"]["run_id"],
        "source_snapshot_identity": c["identities"]["source_snapshot_identity"],
        "nix_store_identity": c["identities"]["nix_store_identity"],
        "podman_store_identity": c["identities"]["podman_store_identity"],
        "full_rebuild_observed": True,
        "resource_bound": {"wall_seconds": 2400, "cpu_quota_cores": 2,
                           "memory_max_bytes": 8589934592, "swap_max_bytes": 0},
        "result": {"service_result": "success", "exit_status": int(u["ExecMainStatus"]),
                   "oom_kills": 0,
                   "last_observed_memory_peak_bytes": int(u["MemoryPeak"]),
                   "last_observed_cpu_nanoseconds": int(u["CPUUsageNSec"])},
        "artifact": artifact(c),
    }

def realized(c):
    out = []
    want = {"*-cargo-nextest-0.9.100": "cargo-nextest", "*-coreutils-9.11": "coreutils",
            "*-rust-script-0.36.0": "rust-script"}
    for pat, name in want.items():
        cands = c["realized_candidates"][pat]
        if len(cands) != 1:
            # coreutils occurs twice (guest package and stdenv bootstrap);
            # the receipt records the guest one, as the prior receipt did.
            prev = {r["name"]: r["path"] for r in old["nix"]["realized_inputs"]}
            cands = [x for x in cands if x["logical"] == prev[name]]
        assert len(cands) == 1, (pat, cands)
        out.append({"name": name, "path": cands[0]["logical"],
                    "nar_hash": cands[0]["narHash"], "nar_size_bytes": cands[0]["narSize"]})
    return out

commit = a["identities"]["commit"]
tree = subprocess.run(["git", "-C", str(wt), "rev-parse", f"{commit}^{{tree}}"],
                      capture_output=True, text=True, check=True).stdout.strip()
archive = subprocess.run(["git", "-C", str(wt), "archive", commit], capture_output=True, check=True).stdout
inputs = []
for p in ["ci/hermetic/build-image.sh", "ci/hermetic/flake.lock", "ci/hermetic/flake.nix", "ci/hermetic/guest-paths.txt"]:
    data = (wt / p).read_bytes()
    inputs.append({"path": p, "sha256": hashlib.sha256(data).hexdigest(), "size_bytes": len(data)})

new = {
    "schema_version": 1,
    "method": old["method"],
    "claim_scope": old["claim_scope"],
    "repin": {
        "superseded_reference": old["repin"]["replacement_reference"],
        "superseded_archive_sha256": old["runs"][0]["artifact"]["archive_sha256"],
        "replacement_reference": "localhost/hermit-hermetic-validate@" + a["loaded"]["loaded_digest"],
        "rationale": ("The 7facbb1c root has no abseil-cpp, googletest or benchmark, so the vendored "
                      "gVisor C++ syscall tests can only resolve those dependencies through host "
                      "pkg-config system libraries and cannot build inside the pinned root. This repin "
                      "moves the pin to the reproducible build of the flake that carries the three "
                      "pinned packages in nativeLibs, so the pinned image is a reproducible pure "
                      "function of its locked inputs and contains every library a gated cell names."),
    },
    "source": {
        "repository": "https://github.com/rrnewton/hermit",
        "git_commit": commit,
        "git_tree": tree,
        "clean": True,
        "git_archive_sha256": hashlib.sha256(archive).hexdigest(),
        "git_archive_size_bytes": len(archive),
        "inputs": inputs,
    },
    "nix": {
        "version": old["nix"]["version"],
        "system": "x86_64-linux",
        "derivation_path": a["derivation_logical"],
        "derivation_sha256": a["derivation_sha256"],
        "output_path": a["output_logical"],
        "output_nar_hash": a["output_nar_hash"],
        "output_nar_size_bytes": a["output_nar_size_bytes"],
        "locked_inputs": old["nix"]["locked_inputs"],
        "realized_inputs": realized(a),
    },
    "evidence_limitations": [
        old["evidence_limitations"][0],
        "Each build substituted only from cache.nixos.org under its trusted key; the removed realized inputs (rust-script, cargo-nextest and coreutils) were rebuilt in each store. Substituted paths are trusted by Nix signature or content hash and NAR hash, not rebuilt.",
        old["evidence_limitations"][2],
        old["evidence_limitations"][3],
        "The superseded 7facbb1c archive was not rebuilt for this receipt; its 92d3399b SHA-256 is carried from the prior receipt that produced the replaced pin.",
        old["evidence_limitations"][5],
    ],
    "runs": [run(a), run(b)],
}
Path(sys.argv[3]).write_text(json.dumps(new, indent=4) + "\n")
print("wrote", sys.argv[3], "replacement:", new["repin"]["replacement_reference"])
