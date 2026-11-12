#!/usr/bin/env python3
"""Collect receipt fields for one isolated build root."""
import hashlib, json, subprocess, sys, tarfile
from pathlib import Path

build = Path(sys.argv[1])
ident = dict(l.split("=", 1) for l in (build / "identities.txt").read_text().splitlines() if "=" in l)
unit = dict(l.split("=", 1) for l in (build / "unit.txt").read_text().splitlines() if "=" in l)
out_line = (build / "output.txt").read_text().splitlines()[0]
out = out_line.split("=", 1)[1]
archive = Path(out)
data = archive.read_bytes()
result = {"identities": ident, "unit": unit,
          "output_physical": out,
          "archive_sha256": hashlib.sha256(data).hexdigest(),
          "archive_size_bytes": len(data)}

store = build / "nix-root/nix"
def nix(*args):
    return subprocess.run(["nix", "--store", str(store), "--extra-experimental-features",
                           "nix-command flakes", *args], capture_output=True, text=True, check=True).stdout
logical_out = "/nix/store/" + Path(out).name
info = json.loads(nix("path-info", "--json", logical_out))[logical_out]
result["output_logical"] = logical_out
result["output_nar_hash"] = info["narHash"]
result["output_nar_size_bytes"] = info["narSize"]
drv = info["deriver"]
result["derivation_logical"] = drv
drv_physical = store / "nix/store" / Path(drv).name
result["derivation_sha256"] = hashlib.sha256(drv_physical.read_bytes()).hexdigest()

# Realized inputs recorded by the receipt: the three unsigned inputs.
store_dir = store / "nix/store"
found = {}
for pat in ("*-cargo-nextest-0.9.100", "*-coreutils-9.11", "*-rust-script-0.36.0"):
    cands = [p for p in store_dir.glob(pat) if p.is_dir() and not p.name.endswith(".drv")]
    assert len(cands) >= 1, (pat, cands)
    for c in cands:
        logical = "/nix/store/" + c.name
        j = json.loads(nix("path-info", "--json", logical))[logical]
        found.setdefault(c.name.split("-", 1)[1].rsplit("-", 1)[0] if False else pat, []).append(
            {"name": c.name, "logical": "/nix/store/" + c.name, "narHash": j["narHash"], "narSize": j["narSize"]})
result["realized_candidates"] = found

loaded = dict(kv.split("=", 1) for kv in (build / "loaded.txt").read_text().split())
result["loaded"] = loaded
_t = tarfile.open(archive)
_m = json.load(_t.extractfile("manifest.json"))[0]
_cfg = _t.extractfile(_m["Config"]).read()
_cfgsha = "sha256:" + hashlib.sha256(_cfg).hexdigest()
_layers = []
for _l in _m["Layers"]:
    _layers.append({"mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
                    "size": _t.getmember(_l).size,
                    "digest": "sha256:" + hashlib.sha256(_t.extractfile(_l).read()).hexdigest()})
_man = {"schemaVersion": 2, "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {"mediaType": "application/vnd.docker.container.image.v1+json",
                   "size": len(_cfg), "digest": _cfgsha}, "layers": _layers}
compact = json.dumps(_man, separators=(",", ":")).encode()
result["manifest_size_bytes"] = len(compact)
result["manifest_compact_sha256"] = "sha256:" + hashlib.sha256(compact).hexdigest()
assert result["manifest_compact_sha256"] == loaded["loaded_digest"], "manifest reconstruction mismatch"

t = tarfile.open(archive)
manifest = json.load(t.extractfile("manifest.json"))[0]
cfg_name = manifest["Config"]
cfg_bytes = t.extractfile(cfg_name).read()
cfg = json.loads(cfg_bytes)
result["config_digest"] = "sha256:" + hashlib.sha256(cfg_bytes).hexdigest()
result["config_size_bytes"] = len(cfg_bytes)
layers = []
for l in manifest["Layers"]:
    layers.append({"member": l, "size": t.getmember(l).size,
                   "sha256": "sha256:" + hashlib.sha256(t.extractfile(l).read()).hexdigest()})
result["layers"] = layers
result["diff_ids"] = cfg["rootfs"]["diff_ids"]
print(json.dumps(result, indent=1))
