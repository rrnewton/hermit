#!/usr/bin/env python3
"""Launch the frozen all-119 adaptive KVM campaign as an attributed bench run.

State-changing ``launch`` must itself enter through INITIAL_BOOTSTRAP with an
independently supplied reviewed SHA-256.  The bootstrap opens the launcher
once, hashes that buffer, and executes the same bytes; direct pathname launch
is intentionally limited to read-only ``status`` and ``static-check``.
"""

from __future__ import annotations

import argparse
import base64
import fcntl
import hashlib
import json
import os
import signal
import shlex
import stat
import struct
import subprocess
import sys
import types
import zlib
from datetime import datetime, timezone
from pathlib import Path


STATE_ROOT = Path("/home/newton/work/dev-hermit")
TOOL_ROOT = STATE_ROOT
EXPECTED_RUN_CHECKOUT = Path(
    "/home/newton/work/dev-hermit/worktrees/slots/kvm-ratchet-90-run6"
)
EXPECTED_RUN_CAMPAIGN = (
    EXPECTED_RUN_CHECKOUT / "ignored/kvm-ratchet-90-qualification-086d41a2"
)
EXECUTED_CAMPAIGN_DIR = globals().get("_KVM_RATCHET_CAMPAIGN_DIR")
CAMPAIGN = (
    Path(EXECUTED_CAMPAIGN_DIR)
    if EXECUTED_CAMPAIGN_DIR is not None
    else Path(__file__).resolve().parent
)
CHECKOUT = CAMPAIGN.parents[1]
RESULTS = CAMPAIGN / "evidence"
PAYLOAD = CAMPAIGN / "run-kvm-ratchet-90.sh"
FREEZE_MANIFEST = CAMPAIGN / "FROZEN_SHA256SUMS"
TARGET = "086d41a2f2f76e5a2cccceea342feb6957311c2b"
TARGET_TREE = "94490fd2cee758395150590a8ba25ff86443dbfd"
AGENT = "kvm-ratchet-90-run6"
EXECUTED_LAUNCHER_SHA256 = globals().get("_KVM_RATCHET_CAPSULE_SHA256")
EXECUTED_LAUNCHER_SOURCE = globals().get("_KVM_RATCHET_CAPSULE_SOURCE")
EXECUTED_FREEZE_MANIFEST_SHA256 = globals().get(
    "_KVM_RATCHET_FREEZE_MANIFEST_SHA256"
)
EXECUTED_FREEZE_MANIFEST_SOURCE = globals().get(
    "_KVM_RATCHET_FREEZE_MANIFEST_SOURCE"
)
MAX_LAUNCHER_BYTES = 262_144
MAX_FREEZE_MANIFEST_BYTES = 65_536
MAX_RUNTIME_INPUT_BYTES = 1_048_576
MAX_RUNTIME_CAPSULE_BYTES = 8_388_608
FIXED_WORKER_PATH = "/home/newton/.cargo/bin:/usr/local/bin:/usr/bin:/bin"
UNIT = "hermit-kvm-ratchet-90-086d41a2-run6"
LOG = STATE_ROOT / "ignored/validate" / f"{UNIT}.log"
WAIT_SECONDS = 1_800
HOLD_SECONDS = 600
CHILD_DEADLINE_SECONDS = 7_200
UNIT_RUNTIME_SECONDS = WAIT_SECONDS + CHILD_DEADLINE_SECONDS + 300
SERVICE_LOG_MAX_BYTES = 1_073_741_824
PROCESS_FILE_LIMIT_BYTES = 1_073_742_000
CAMPAIGN_BUDGET_BYTES = 137_438_953_472
FILESYSTEM_RESERVE_BYTES = 137_438_953_472
DEADLINE_BASIS = (
    "prior 120 outer/130 attempt campaign=1169s; adaptive max=357 outer/595 attempts; "
    "retry-aware projection=5350s; 7200s is a deliberate ~1.35x operational "
    "stop-for-investigation covering fetch/build/119 serial preparations/finalization; "
    "each outer invocation retains a 600s infrastructure-only backstop above its "
    "~248s two-attempt modeled path"
)
FROZEN_HASHES = {
    "run-kvm-ratchet-90.sh": "368e27b9bd56b3877fea838e365e77c6dec2a5a96c98849131b65b65e1e04c99",
    "validate-evidence.sh": "51d84298cb45d7e6ddec2cc9453db1147112aec7c6a24acf5f8342c9bd61a1ad",
    "validate-results.jq": "2f06da4497ea1dfd19c144b39b54a5db54f912b6d3f871f21e27fa781635753c",
    "validate-strict-invocation-artifacts.sh": "b1d3a0ab1e686160f7da617f871a68bb93780b6463e0c57ec105350be43a465a",
    "validate-population.jq": "e9769e1f46b04118d242fda5cd6f66f893065cd3163b410c90877ac43b0cd484",
    "expected-cells.json": "ce945b0e134dc6ca0b7089d3c439d00672d322a94f5efc8730bf18cb59fa4e72",
    "self-test.py": "87e3108dae9ab71a1a8d3ff0becee7054d875664559953098aad5a2d424eaa74",
}
RUNTIME_INPUT_NAMES = (
    "FROZEN_SHA256SUMS",
    "run-kvm-ratchet-90.sh",
    "expected-cells.json",
    "validate-population.jq",
    "validate-results.jq",
    "validate-evidence.sh",
    "validate-strict-invocation-artifacts.sh",
)
RUNTIME_PATH_ENV = {
    "FROZEN_SHA256SUMS": "KVM_RATCHET_FREEZE_MANIFEST_PATH",
    "run-kvm-ratchet-90.sh": "KVM_RATCHET_PAYLOAD_PATH",
    "expected-cells.json": "KVM_RATCHET_EXPECTED_CELLS_PATH",
    "validate-population.jq": "KVM_RATCHET_POPULATION_VALIDATOR_PATH",
    "validate-results.jq": "KVM_RATCHET_RESULTS_VALIDATOR_PATH",
    "validate-evidence.sh": "KVM_RATCHET_EVIDENCE_VALIDATOR_PATH",
    "validate-strict-invocation-artifacts.sh":
        "KVM_RATCHET_STRICT_ARTIFACT_VALIDATOR_PATH",
}
INITIAL_BOOTSTRAP = r'''import hashlib,hmac,os,stat,sys
def read_once(path,label,limit):
    fd=os.open(path,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW)
    try:
        metadata=os.fstat(fd)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
            raise SystemExit(label+" is not one unaliased regular file")
        if metadata.st_size > limit:
            raise SystemExit(label+" exceeds its byte limit")
        chunks=[]
        total=0
        while True:
            chunk=os.read(fd,min(1024*1024,limit+1-total))
            if not chunk:
                break
            total+=len(chunk)
            if total > limit:
                raise SystemExit(label+" exceeds its byte limit")
            chunks.append(chunk)
        return b"".join(chunks)
    finally:
        os.close(fd)
expected_launcher=sys.argv.pop(1)
expected_freeze=sys.argv.pop(1)
source_name=sys.argv.pop(1)
freeze_name=sys.argv.pop(1)
campaign_dir=sys.argv.pop(1)
if os.path.abspath(source_name) != os.path.join(campaign_dir,"launch.py") or os.path.abspath(freeze_name) != os.path.join(campaign_dir,"FROZEN_SHA256SUMS"):
    raise SystemExit("trusted launcher paths do not match the external campaign directory")
source=read_once(source_name,"launcher",262144)
freeze_source=read_once(freeze_name,"freeze manifest",65536)
launcher_digest=hashlib.sha256(source).hexdigest()
freeze_digest=hashlib.sha256(freeze_source).hexdigest()
if not hmac.compare_digest(launcher_digest,expected_launcher):
    raise SystemExit("launcher digest mismatch")
if not hmac.compare_digest(freeze_digest,expected_freeze):
    raise SystemExit("freeze manifest digest mismatch")
sys.argv[0]=source_name
namespace={"__name__":"__main__","__file__":source_name,"_KVM_RATCHET_CAMPAIGN_DIR":campaign_dir,"_KVM_RATCHET_CAPSULE_SHA256":launcher_digest,"_KVM_RATCHET_CAPSULE_SOURCE":source,"_KVM_RATCHET_FREEZE_MANIFEST_SHA256":freeze_digest,"_KVM_RATCHET_FREEZE_MANIFEST_SOURCE":freeze_source}
exec(compile(source,source_name,"exec"),namespace)
'''
WORKER_BOOTSTRAP = r'''import base64,datetime,fcntl,hashlib,hmac,json,os,stat,sys,tempfile,zlib
record_name=sys.argv.pop(1)
expected_unit=sys.argv.pop(1)
expected_target=sys.argv.pop(1)
encoded_source=sys.argv.pop(1)
expected_launcher=sys.argv.pop(1)
expected_freeze=sys.argv.pop(1)
encoded_freeze=sys.argv.pop(1)
source_name=sys.argv.pop(1)
campaign_dir=sys.argv.pop(1)
def terminalize(detail):
    try:
        parent=os.path.dirname(record_name)
        lock_name=os.path.join(parent,"."+os.path.basename(record_name)+".lock")
        os.makedirs(parent,exist_ok=True)
        with open(lock_name,"a+",encoding="utf-8") as lock:
            fcntl.flock(lock.fileno(),fcntl.LOCK_EX)
            fd=os.open(record_name,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW)
            try:
                metadata=os.fstat(fd)
                if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
                    raise RuntimeError("run record is not one unaliased regular file")
                chunks=[]
                while True:
                    chunk=os.read(fd,1024*1024)
                    if not chunk:
                        break
                    chunks.append(chunk)
            finally:
                os.close(fd)
            value=json.loads(b"".join(chunks))
            if value.get("unit") != expected_unit or value.get("target") != expected_target or value.get("kind") != "bench":
                raise RuntimeError("run record identity changed")
            if value.get("state") not in ("launching","running"):
                return False
            value.update({"state":"failed","result":"failed","exit_code":2,"detail":detail,"finished_at":datetime.datetime.now(datetime.timezone.utc).isoformat()})
            out_fd,temporary=tempfile.mkstemp(prefix="."+os.path.basename(record_name)+".",dir=parent)
            try:
                with os.fdopen(out_fd,"w",encoding="utf-8") as stream:
                    json.dump(value,stream,indent=2,sort_keys=True)
                    stream.write("\n")
                    stream.flush()
                    os.fsync(stream.fileno())
                os.replace(temporary,record_name)
                directory_fd=os.open(parent,os.O_RDONLY|os.O_DIRECTORY)
                try:
                    os.fsync(directory_fd)
                finally:
                    os.close(directory_fd)
            finally:
                try:
                    os.unlink(temporary)
                except FileNotFoundError:
                    pass
            return True
    except Exception as update_error:
        print("worker bootstrap could not publish terminal failure: "+str(update_error),file=sys.stderr)
        return False
try:
    if os.path.abspath(source_name) != os.path.join(campaign_dir,"launch.py"):
        raise RuntimeError("worker launcher label does not match the trusted campaign directory")
    if len(encoded_source) > 524288 or len(encoded_freeze) > 131072:
        raise RuntimeError("worker bootstrap capsule exceeds its encoded byte limit")
    compressed_source=base64.b64decode(encoded_source,validate=True)
    decompressor=zlib.decompressobj()
    source=decompressor.decompress(compressed_source,262145)
    if len(source) > 262144 or decompressor.unconsumed_tail or not decompressor.eof or decompressor.unused_data:
        raise RuntimeError("launcher capsule exceeds its decoded byte limit")
    freeze_source=base64.b64decode(encoded_freeze,validate=True)
    if len(freeze_source) > 65536:
        raise RuntimeError("freeze manifest capsule exceeds its decoded byte limit")
    launcher_digest=hashlib.sha256(source).hexdigest()
    freeze_digest=hashlib.sha256(freeze_source).hexdigest()
    if not hmac.compare_digest(launcher_digest,expected_launcher):
        raise RuntimeError("launcher capsule digest mismatch")
    if not hmac.compare_digest(freeze_digest,expected_freeze):
        raise RuntimeError("freeze manifest capsule digest mismatch")
    sys.argv[0]=source_name
    namespace={"__name__":"__main__","__file__":source_name,"_KVM_RATCHET_CAMPAIGN_DIR":campaign_dir,"_KVM_RATCHET_CAPSULE_SHA256":launcher_digest,"_KVM_RATCHET_CAPSULE_SOURCE":source,"_KVM_RATCHET_FREEZE_MANIFEST_SHA256":freeze_digest,"_KVM_RATCHET_FREEZE_MANIFEST_SOURCE":freeze_source}
    exec(compile(source,source_name,"exec"),namespace)
except SystemExit as stopped:
    code=stopped.code if isinstance(stopped.code,int) else 1
    if code != 0:
        terminalize("trusted worker exited before publishing a terminal record")
    elif terminalize("trusted worker returned success without publishing a terminal record"):
        code=2
    raise SystemExit(code)
except BaseException as error:
    terminalize("trusted worker bootstrap failed: "+str(error))
    raise
'''
POST_BOUNDARY_BOOTSTRAP = r'''import base64,fcntl,hashlib,hmac,json,os,sys,zlib
names=("FROZEN_SHA256SUMS","run-kvm-ratchet-90.sh","expected-cells.json","validate-population.jq","validate-results.jq","validate-evidence.sh","validate-strict-invocation-artifacts.sh")
path_environment={"FROZEN_SHA256SUMS":"KVM_RATCHET_FREEZE_MANIFEST_PATH","run-kvm-ratchet-90.sh":"KVM_RATCHET_PAYLOAD_PATH","expected-cells.json":"KVM_RATCHET_EXPECTED_CELLS_PATH","validate-population.jq":"KVM_RATCHET_POPULATION_VALIDATOR_PATH","validate-results.jq":"KVM_RATCHET_RESULTS_VALIDATOR_PATH","validate-evidence.sh":"KVM_RATCHET_EVIDENCE_VALIDATOR_PATH","validate-strict-invocation-artifacts.sh":"KVM_RATCHET_STRICT_ARTIFACT_VALIDATOR_PATH"}
if len(sys.argv) != 9:
    raise SystemExit("post-boundary bootstrap requires exactly eight arguments")
capsule,capsule_sha256,expected_hashes_text,payload_sha256,freeze_sha256,campaign_dir,service_log,payload_guard=sys.argv[1:]
def valid_digest(value):
    return isinstance(value,str) and len(value) == 64 and all(character in "0123456789abcdef" for character in value)
def unique_object(pairs):
    value={}
    for key,member in pairs:
        if key in value:
            raise ValueError("duplicate JSON member: "+str(key))
        value[key]=member
    return value
if not all(valid_digest(value) for value in (capsule_sha256,payload_sha256,freeze_sha256)):
    raise SystemExit("post-boundary bootstrap digest argument is malformed")
try:
    capsule_bytes=capsule.encode("ascii")
except UnicodeEncodeError as error:
    raise SystemExit("runtime input capsule is not ASCII") from error
if len(capsule_bytes) > 8388608:
    raise SystemExit("runtime input capsule exceeds its encoded byte limit")
observed_capsule_sha256=hashlib.sha256(capsule_bytes).hexdigest()
if not hmac.compare_digest(observed_capsule_sha256,capsule_sha256):
    raise SystemExit("runtime input capsule transport digest mismatch")
try:
    expected_hashes=json.loads(expected_hashes_text,object_pairs_hook=unique_object)
except Exception as error:
    raise SystemExit("runtime input digest map is malformed: "+str(error)) from error
if not isinstance(expected_hashes,dict) or set(expected_hashes) != set(names) or not all(valid_digest(value) for value in expected_hashes.values()):
    raise SystemExit("runtime input digest map is incomplete, unexpected, or malformed")
if not hmac.compare_digest(expected_hashes["FROZEN_SHA256SUMS"],freeze_sha256) or not hmac.compare_digest(expected_hashes["run-kvm-ratchet-90.sh"],payload_sha256):
    raise SystemExit("runtime input digest anchors disagree")
try:
    compressed=base64.b64decode(capsule_bytes,validate=True)
    decompressor=zlib.decompressobj()
    serialized=decompressor.decompress(compressed,8388609)
    if len(serialized) > 8388608 or decompressor.unconsumed_tail or not decompressor.eof or decompressor.unused_data:
        raise ValueError("decoded capsule exceeds its byte limit or framing")
    document=json.loads(serialized,object_pairs_hook=unique_object)
except Exception as error:
    raise SystemExit("runtime input capsule is malformed: "+str(error)) from error
if not isinstance(document,dict) or set(document) != set(names):
    raise SystemExit("runtime input capsule set is incomplete or unexpected")
captured={}
for name in names:
    encoded=document[name]
    if not isinstance(encoded,str):
        raise SystemExit("runtime input capsule value is not text: "+name)
    try:
        content=base64.b64decode(encoded,validate=True)
    except Exception as error:
        raise SystemExit("runtime input capsule bytes are malformed: "+name+": "+str(error)) from error
    member_limit=65536 if name == "FROZEN_SHA256SUMS" else 1048576
    if len(content) > member_limit:
        raise SystemExit("runtime input capsule member exceeds its byte limit: "+name)
    observed=hashlib.sha256(content).hexdigest()
    if not hmac.compare_digest(observed,expected_hashes[name]):
        raise SystemExit("runtime input capsule hash mismatch: "+name)
    captured[name]=content
required_seals=fcntl.F_SEAL_WRITE|fcntl.F_SEAL_GROW|fcntl.F_SEAL_SHRINK|fcntl.F_SEAL_SEAL
fds=[]
paths={}
try:
    for name in names:
        low_fd=os.memfd_create("kvm-ratchet-"+name,os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
        high_fd=-1
        try:
            view=memoryview(captured[name])
            written=0
            while written < len(view):
                count=os.write(low_fd,view[written:])
                if count <= 0:
                    raise RuntimeError("short write while recreating "+name)
                written+=count
            os.lseek(low_fd,0,os.SEEK_SET)
            fcntl.fcntl(low_fd,fcntl.F_ADD_SEALS,required_seals)
            if fcntl.fcntl(low_fd,fcntl.F_GET_SEALS)&required_seals != required_seals:
                raise RuntimeError("runtime input snapshot is not fully sealed: "+name)
            high_fd=fcntl.fcntl(low_fd,fcntl.F_DUPFD_CLOEXEC,100)
        finally:
            os.close(low_fd)
        try:
            os.set_inheritable(high_fd,True)
            if not os.get_inheritable(high_fd):
                raise RuntimeError("runtime input snapshot is not inheritable: "+name)
            if fcntl.fcntl(high_fd,fcntl.F_GET_SEALS)&required_seals != required_seals:
                raise RuntimeError("duplicated runtime input snapshot lost its seals: "+name)
        except BaseException:
            os.close(high_fd)
            raise
        fds.append(high_fd)
        paths[name]="/proc/self/fd/"+str(high_fd)
    allowed_environment=("HOME","PATH","PYTHONUNBUFFERED","DEV_HERMIT_PARENT","DEV_HERMIT_TOOL_ROOT","RUSTUP_TOOLCHAIN","XDG_RUNTIME_DIR","http_proxy","https_proxy","ftp_proxy","HTTP_PROXY","HTTPS_PROXY","FTP_PROXY","no_proxy","CI_HUB_PAYLOAD_DOMAIN","CI_HUB_VALIDATE_LOCK_OWNER_PID","CI_HUB_VALIDATE_LOCK_OWNER_FILE","CI_HUB_VALIDATE_RUN_NUMBER","CI_HUB_VALIDATE_BRANCH")
    environment={name:os.environ[name] for name in allowed_environment if name in os.environ}
    required_environment=("HOME","PATH","PYTHONUNBUFFERED","DEV_HERMIT_PARENT","DEV_HERMIT_TOOL_ROOT","RUSTUP_TOOLCHAIN","XDG_RUNTIME_DIR","http_proxy","https_proxy","ftp_proxy","HTTP_PROXY","HTTPS_PROXY","FTP_PROXY","no_proxy","CI_HUB_PAYLOAD_DOMAIN","CI_HUB_VALIDATE_LOCK_OWNER_PID","CI_HUB_VALIDATE_LOCK_OWNER_FILE")
    missing_environment=[name for name in required_environment if name not in environment]
    if missing_environment:
        raise RuntimeError("post-boundary environment is missing required authority: "+",".join(missing_environment))
    environment.update({path_environment[name]:paths[name] for name in names})
    environment.update({"KVM_RATCHET_CAMPAIGN_DIR":campaign_dir,"KVM_RATCHET_INPUT_SOURCE":"sealed-memfd","KVM_RATCHET_FREEZE_MANIFEST_SHA256":freeze_sha256,"KVM_RATCHET_SERVICE_LOG_PATH":service_log})
    environment_arguments=[path_environment[name]+"="+paths[name] for name in names]
    environment_arguments.append("KVM_RATCHET_SERVICE_LOG_PATH="+service_log)
    print("POST_BOUNDARY_FROZEN_INPUTS "+json.dumps({"fd_count":len(fds),"runtime_capsule_sha256":observed_capsule_sha256,"runtime_input_hashes":expected_hashes},sort_keys=True,separators=(",",":")),flush=True)
    os.execve("/bin/bash",["/bin/bash","-c",payload_guard,"kvm-ratchet-payload-guard",payload_sha256,paths["run-kvm-ratchet-90.sh"],campaign_dir,freeze_sha256,*environment_arguments],environment)
except BaseException:
    for descriptor in fds:
        try:
            os.close(descriptor)
        except OSError:
            pass
    raise
'''
PAYLOAD_GUARD = r'''set -euo pipefail
expected=$1
payload=$2
campaign_dir=$3
freeze_manifest_sha256=$4
shift 4
hash_line=$(/usr/bin/sha256sum -- "$payload")
actual=${hash_line%% *}
if [[ "$actual" != "$expected" ]]; then
  printf 'QUEUE_PAYLOAD_CHECK mismatch expected=%s actual=%s path=%s\n' \
    "$expected" "$actual" "$payload" >&2
  exit 75
fi
printf 'QUEUE_PAYLOAD_CHECK ok expected=%s actual=%s path=%s\n' \
  "$expected" "$actual" "$payload"
exec /usr/bin/env \
  KVM_RATCHET_PAYLOAD_SHA256="$expected" \
  KVM_RATCHET_CAMPAIGN_DIR="$campaign_dir" \
  KVM_RATCHET_INPUT_SOURCE=sealed-memfd \
  KVM_RATCHET_FREEZE_MANIFEST_SHA256="$freeze_manifest_sha256" \
  "$@" /bin/bash "$payload"
'''

TRUSTED_RUN_REGISTRY_MODULES = {
    "final_validate_status.py": "d784565d46330d56b81ace62644c2c4891bffaa561f1d2d756a081ca2c2219c0",
    "service_result.py": "2c7ab5aaa77304a76c0517b6d2f2f6fcf853e54beacac93cae712a0857eec282",
    "run_registry.py": "d6d614fbeb7255598b5130d1780b73c6f794edb72ac4b8d7d428c1f2e6c35c85",
}


def load_trusted_run_registry():
    module_root = TOOL_ROOT / "ci-hub/validate"
    loaded = {}
    for filename in (
        "final_validate_status.py",
        "service_result.py",
        "run_registry.py",
    ):
        path = module_root / filename
        descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
        try:
            metadata = os.fstat(descriptor)
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
                raise RuntimeError(
                    f"run-registry dependency is not one unaliased regular file: {path}"
                )
            if metadata.st_size > MAX_RUNTIME_INPUT_BYTES:
                raise RuntimeError(
                    f"run-registry dependency exceeds its byte limit: {path}"
                )
            chunks = []
            total = 0
            while chunk := os.read(
                descriptor,
                min(1024 * 1024, MAX_RUNTIME_INPUT_BYTES + 1 - total),
            ):
                total += len(chunk)
                if total > MAX_RUNTIME_INPUT_BYTES:
                    raise RuntimeError(
                        f"run-registry dependency exceeds its byte limit: {path}"
                    )
                chunks.append(chunk)
            source = b"".join(chunks)
        finally:
            os.close(descriptor)
        observed = hashlib.sha256(source).hexdigest()
        expected = TRUSTED_RUN_REGISTRY_MODULES[filename]
        if observed != expected:
            raise RuntimeError(
                f"run-registry dependency changed: {filename}: "
                f"expected {expected}, got {observed}"
            )
        module_name = filename.removesuffix(".py")
        module = types.ModuleType(module_name)
        module.__file__ = str(path)
        module.__package__ = ""
        sys.modules[module_name] = module
        exec(compile(source, str(path), "exec"), module.__dict__)
        loaded[module_name] = module
    return loaded["run_registry"]


run_registry = load_trusted_run_registry()

RECORD = run_registry.record_path(STATE_ROOT, UNIT)


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def read_unaliased_regular_file(
    path: Path, *, max_bytes: int = MAX_RUNTIME_INPUT_BYTES
) -> bytes:
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
            raise RuntimeError(f"input is not one unaliased regular file: {path}")
        if metadata.st_size > max_bytes:
            raise RuntimeError(f"input exceeds its byte limit: {path}")
        chunks: list[bytes] = []
        total = 0
        while chunk := os.read(
            descriptor, min(1024 * 1024, max_bytes + 1 - total)
        ):
            total += len(chunk)
            if total > max_bytes:
                raise RuntimeError(f"input exceeds its byte limit: {path}")
            chunks.append(chunk)
        return b"".join(chunks)
    finally:
        os.close(descriptor)


def initial_record() -> dict[str, object]:
    return {
        "schema_version": run_registry.SCHEMA_VERSION,
        "kind": "bench",
        "state": "launching",
        "unit": f"{UNIT}.service",
        "target": TARGET,
        "repo": "rrnewton/hermit",
        "checkout": str(CHECKOUT),
        "log": str(LOG),
        "agent": AGENT,
        "started_at": utc_now(),
        "producer": run_registry.PRODUCER,
        "admission": "ci-hub validate-lock",
    }


def runtime_input_paths() -> dict[str, Path]:
    return {name: CAMPAIGN / name for name in RUNTIME_INPUT_NAMES}


def create_sealed_input(
    name: str, content: bytes, expected_sha256: str
) -> tuple[int, str]:
    observed_sha256 = hashlib.sha256(content).hexdigest()
    if observed_sha256 != expected_sha256:
        raise RuntimeError(
            f"runtime input changed: {name}: expected {expected_sha256}, "
            f"got {observed_sha256}"
        )
    snapshot_fd = os.memfd_create(
        f"kvm-ratchet-{name}", os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING
    )
    try:
        view = memoryview(content)
        written = 0
        while written < len(view):
            count = os.write(snapshot_fd, view[written:])
            if count <= 0:
                raise RuntimeError(f"short write while snapshotting {name}")
            written += count
        os.lseek(snapshot_fd, 0, os.SEEK_SET)
        required_seals = (
            fcntl.F_SEAL_WRITE
            | fcntl.F_SEAL_GROW
            | fcntl.F_SEAL_SHRINK
            | fcntl.F_SEAL_SEAL
        )
        fcntl.fcntl(snapshot_fd, fcntl.F_ADD_SEALS, required_seals)
        observed_seals = fcntl.fcntl(snapshot_fd, fcntl.F_GET_SEALS)
        if observed_seals & required_seals != required_seals:
            raise RuntimeError(f"runtime input snapshot is not fully sealed: {name}")
        high_fd = fcntl.fcntl(snapshot_fd, fcntl.F_DUPFD_CLOEXEC, 100)
        os.close(snapshot_fd)
        snapshot_fd = high_fd
        os.set_inheritable(snapshot_fd, True)
        return snapshot_fd, f"/proc/self/fd/{snapshot_fd}"
    except Exception:
        os.close(snapshot_fd)
        raise


def capture_runtime_input_bytes(
    paths: dict[str, Path] | None = None,
    expected_hashes: dict[str, str] | None = None,
    *,
    freeze_manifest_source: bytes | None = None,
    freeze_manifest_sha256: str | None = None,
) -> dict[str, bytes]:
    source_paths = paths or runtime_input_paths()
    hashes = expected_hashes or FROZEN_HASHES
    if set(source_paths) != set(RUNTIME_INPUT_NAMES):
        raise RuntimeError("runtime input capture set is incomplete or unexpected")
    captured: dict[str, bytes] = {}
    for name in RUNTIME_INPUT_NAMES:
        content = (
            freeze_manifest_source
            if name == FREEZE_MANIFEST.name and freeze_manifest_source is not None
            else read_unaliased_regular_file(
                source_paths[name],
                max_bytes=(
                    MAX_FREEZE_MANIFEST_BYTES
                    if name == FREEZE_MANIFEST.name
                    else MAX_RUNTIME_INPUT_BYTES
                ),
            )
        )
        if name == FREEZE_MANIFEST.name:
            observed = hashlib.sha256(content).hexdigest()
            if freeze_manifest_sha256 is not None and observed != freeze_manifest_sha256:
                raise RuntimeError(
                    "freeze manifest buffer disagrees with its external trust anchor: "
                    f"expected {freeze_manifest_sha256}, got {observed}"
                )
            captured[name] = content
            continue
        expected = hashes.get(name)
        if expected is None:
            raise RuntimeError(f"runtime input has no frozen digest: {name}")
        observed = hashlib.sha256(content).hexdigest()
        if observed != expected:
            raise RuntimeError(
                f"runtime input changed: {name}: expected {expected}, got {observed}"
            )
        captured[name] = content
    return captured


def encode_runtime_input_capsule(captured: dict[str, bytes]) -> str:
    if set(captured) != set(RUNTIME_INPUT_NAMES):
        raise RuntimeError("runtime input capsule set is incomplete or unexpected")
    if any(
        len(captured[name])
        > (
            MAX_FREEZE_MANIFEST_BYTES
            if name == FREEZE_MANIFEST.name
            else MAX_RUNTIME_INPUT_BYTES
        )
        for name in captured
    ):
        raise RuntimeError("runtime input capsule member exceeds its byte limit")
    document = {
        name: base64.b64encode(captured[name]).decode("ascii")
        for name in RUNTIME_INPUT_NAMES
    }
    serialized = json.dumps(
        document, sort_keys=True, separators=(",", ":")
    ).encode()
    if len(serialized) > MAX_RUNTIME_CAPSULE_BYTES:
        raise RuntimeError("runtime input capsule exceeds its decoded byte limit")
    return base64.b64encode(zlib.compress(serialized, level=9)).decode("ascii")


def unique_json_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON member: {key}")
        result[key] = value
    return result


def decode_runtime_input_capsule(
    capsule: str,
    freeze_manifest_sha256: str,
    expected_hashes: dict[str, str] | None = None,
) -> dict[str, bytes]:
    hashes = expected_hashes or FROZEN_HASHES
    try:
        if len(capsule) > MAX_RUNTIME_CAPSULE_BYTES:
            raise ValueError("encoded capsule exceeds its byte limit")
        compressed = base64.b64decode(capsule, validate=True)
        decompressor = zlib.decompressobj()
        serialized = decompressor.decompress(
            compressed, MAX_RUNTIME_CAPSULE_BYTES + 1
        )
        if (
            len(serialized) > MAX_RUNTIME_CAPSULE_BYTES
            or decompressor.unconsumed_tail
            or not decompressor.eof
            or decompressor.unused_data
        ):
            raise ValueError("decoded capsule exceeds its byte limit or framing")
        document = json.loads(serialized, object_pairs_hook=unique_json_object)
    except Exception as error:
        raise RuntimeError(f"runtime input capsule is malformed: {error}") from error
    if not isinstance(document, dict) or set(document) != set(RUNTIME_INPUT_NAMES):
        raise RuntimeError("runtime input capsule set is incomplete or unexpected")
    captured: dict[str, bytes] = {}
    for name in RUNTIME_INPUT_NAMES:
        encoded = document[name]
        if not isinstance(encoded, str):
            raise RuntimeError(f"runtime input capsule value is not text: {name}")
        try:
            content = base64.b64decode(encoded, validate=True)
        except Exception as error:
            raise RuntimeError(
                f"runtime input capsule bytes are malformed: {name}: {error}"
            ) from error
        member_limit = (
            MAX_FREEZE_MANIFEST_BYTES
            if name == FREEZE_MANIFEST.name
            else MAX_RUNTIME_INPUT_BYTES
        )
        if len(content) > member_limit:
            raise RuntimeError(
                f"runtime input capsule member exceeds its byte limit: {name}"
            )
        observed = hashlib.sha256(content).hexdigest()
        expected = (
            freeze_manifest_sha256
            if name == FREEZE_MANIFEST.name
            else hashes.get(name)
        )
        if expected is None or observed != expected:
            raise RuntimeError(
                f"runtime input capsule hash mismatch: {name}: "
                f"expected {expected}, got {observed}"
            )
        captured[name] = content
    return captured


def create_runtime_input_snapshots(
    captured: dict[str, bytes],
    freeze_manifest_sha256: str,
    expected_hashes: dict[str, str] | None = None,
) -> dict[str, object]:
    hashes = expected_hashes or FROZEN_HASHES
    if set(captured) != set(RUNTIME_INPUT_NAMES):
        raise RuntimeError("runtime input snapshot set is incomplete or unexpected")
    fds: list[int] = []
    snapshot_paths: dict[str, Path] = {}
    try:
        for name in RUNTIME_INPUT_NAMES:
            expected = (
                freeze_manifest_sha256
                if name == FREEZE_MANIFEST.name
                else hashes.get(name)
            )
            if expected is None:
                raise RuntimeError(f"runtime input has no frozen digest: {name}")
            fd, fd_path = create_sealed_input(name, captured[name], expected)
            fds.append(fd)
            snapshot_paths[name] = Path(fd_path)
    except Exception:
        for fd in fds:
            os.close(fd)
        raise
    return {
        "fds": tuple(fds),
        "paths": snapshot_paths,
        "hashes": {
            name: hashlib.sha256(captured[name]).hexdigest()
            for name in RUNTIME_INPUT_NAMES
        },
    }


def close_runtime_input_snapshots(snapshot: dict[str, object]) -> None:
    for fd in snapshot["fds"]:
        os.close(fd)


def runtime_input_environment(paths: dict[str, Path]) -> dict[str, str]:
    environment = {
        "KVM_RATCHET_CAMPAIGN_DIR": str(CAMPAIGN),
        "KVM_RATCHET_INPUT_SOURCE": "sealed-memfd",
    }
    environment.update(
        {RUNTIME_PATH_ENV[name]: str(paths[name]) for name in RUNTIME_INPUT_NAMES}
    )
    return environment


def guarded_payload_command(
    payload: Path,
    expected_sha256: str,
    freeze_manifest_sha256: str,
    snapshot_paths: dict[str, Path] | None = None,
) -> list[str]:
    paths = snapshot_paths or runtime_input_paths()
    environment_arguments = [
        f"{RUNTIME_PATH_ENV[name]}={paths[name]}" for name in RUNTIME_INPUT_NAMES
    ]
    environment_arguments.append(f"KVM_RATCHET_SERVICE_LOG_PATH={LOG}")
    return [
        "/bin/bash",
        "-c",
        PAYLOAD_GUARD,
        "kvm-ratchet-payload-guard",
        expected_sha256,
        str(payload),
        str(CAMPAIGN),
        freeze_manifest_sha256,
        *environment_arguments,
    ]


def runtime_input_digest_map(
    freeze_manifest_sha256: str,
    expected_hashes: dict[str, str] | None = None,
) -> dict[str, str]:
    hashes = expected_hashes or FROZEN_HASHES
    result = {
        name: (
            freeze_manifest_sha256
            if name == FREEZE_MANIFEST.name
            else hashes.get(name)
        )
        for name in RUNTIME_INPUT_NAMES
    }
    if any(
        not isinstance(digest, str)
        or len(digest) != 64
        or any(character not in "0123456789abcdef" for character in digest)
        for digest in result.values()
    ):
        raise RuntimeError("runtime input digest map is incomplete or malformed")
    return result


def runtime_capsule_sha256(runtime_capsule: str) -> str:
    try:
        encoded = runtime_capsule.encode("ascii")
    except UnicodeEncodeError as error:
        raise RuntimeError("runtime input capsule is not ASCII") from error
    if len(encoded) > MAX_RUNTIME_CAPSULE_BYTES:
        raise RuntimeError("runtime input capsule exceeds its encoded byte limit")
    return hashlib.sha256(encoded).hexdigest()


def post_boundary_payload_command(
    *,
    runtime_capsule: str,
    runtime_capsule_digest: str,
    payload_sha256: str,
    freeze_manifest_sha256: str,
    expected_hashes: dict[str, str] | None = None,
) -> list[str]:
    digest_map = runtime_input_digest_map(
        freeze_manifest_sha256,
        expected_hashes,
    )
    if digest_map[PAYLOAD.name] != payload_sha256:
        raise RuntimeError("payload digest disagrees with the runtime input digest map")
    observed_capsule_digest = runtime_capsule_sha256(runtime_capsule)
    if observed_capsule_digest != runtime_capsule_digest:
        raise RuntimeError("runtime capsule disagrees with its pre-boundary digest")
    return [
        "/usr/bin/python3",
        "-I",
        "-S",
        "-c",
        POST_BOUNDARY_BOOTSTRAP,
        runtime_capsule,
        runtime_capsule_digest,
        json.dumps(digest_map, sort_keys=True, separators=(",", ":")),
        payload_sha256,
        freeze_manifest_sha256,
        str(CAMPAIGN),
        str(LOG),
        PAYLOAD_GUARD,
    ]


def exec_vector_measurement(
    command: list[str], environment: dict[str, str]
) -> dict[str, int]:
    argument_sizes = [len(os.fsencode(argument)) + 1 for argument in command]
    environment_sizes = [
        len(os.fsencode(name)) + len(os.fsencode(value)) + 2
        for name, value in environment.items()
    ]
    pointer_bytes = (len(command) + len(environment) + 2) * struct.calcsize("P")
    return {
        "argument_bytes": sum(argument_sizes),
        "environment_bytes": sum(environment_sizes),
        "pointer_bytes": pointer_bytes,
        "total_bytes": sum(argument_sizes) + sum(environment_sizes) + pointer_bytes,
        "max_argument_bytes": max(argument_sizes, default=0) - 1,
        "max_argument_limit": 32 * os.sysconf("SC_PAGE_SIZE") - 1,
        "arg_max": os.sysconf("SC_ARG_MAX"),
    }


def require_exec_vector_budget(command: list[str], label: str) -> dict[str, int]:
    measurement = exec_vector_measurement(command, dict(os.environ))
    if measurement["max_argument_bytes"] > measurement["max_argument_limit"]:
        raise RuntimeError(f"{label} has an argument above Linux MAX_ARG_STRLEN")
    conservative_limit = min(measurement["arg_max"] // 2, 512 * 1024)
    if (
        measurement["argument_bytes"] >= conservative_limit
        or measurement["total_bytes"] >= measurement["arg_max"]
    ):
        raise RuntimeError(
            f"{label} is too large: {measurement['argument_bytes']} argument bytes, "
            f"{measurement['environment_bytes']} environment bytes, "
            f"{measurement['pointer_bytes']} pointer bytes "
            f"(conservative argument limit {conservative_limit}, "
            f"ARG_MAX {measurement['arg_max']})"
        )
    return measurement


def worker_command(
    *,
    payload_sha256: str,
    freeze_manifest_sha256: str,
    runtime_capsule: str,
    runtime_capsule_digest: str,
) -> list[str]:
    command = [
        str(TOOL_ROOT / "ci-hub/ci-hub"),
        "validate-lock",
        "run",
        "--agent",
        AGENT,
        "--kind",
        "bench",
        "--target",
        TARGET,
        "--run-record",
        str(RECORD),
        "--wait",
        str(WAIT_SECONDS),
        "--hold",
        str(HOLD_SECONDS),
        "--child-deadline",
        str(CHILD_DEADLINE_SECONDS),
        "--",
    ] + post_boundary_payload_command(
        runtime_capsule=runtime_capsule,
        runtime_capsule_digest=runtime_capsule_digest,
        payload_sha256=payload_sha256,
        freeze_manifest_sha256=freeze_manifest_sha256,
    )
    require_exec_vector_budget(command, "post-boundary worker command")
    return command


def launcher_capsule(source: bytes) -> str:
    if len(source) > MAX_LAUNCHER_BYTES:
        raise RuntimeError("launcher source exceeds its byte limit")
    encoded = base64.b64encode(zlib.compress(source, level=9)).decode("ascii")
    if len(encoded) > 524_288:
        raise RuntimeError("launcher capsule exceeds its encoded byte limit")
    return encoded


def trusted_entry_command(
    launcher_sha256: str,
    freeze_manifest_sha256: str,
    *arguments: str,
) -> list[str]:
    for name, digest in (
        ("launcher", launcher_sha256),
        ("freeze manifest", freeze_manifest_sha256),
    ):
        if len(digest) != 64 or any(character not in "0123456789abcdef" for character in digest):
            raise RuntimeError(f"{name} external trust digest is malformed")
    return [
        "/usr/bin/python3",
        "-I",
        "-S",
        "-c",
        INITIAL_BOOTSTRAP,
        launcher_sha256,
        freeze_manifest_sha256,
        str(EXPECTED_RUN_CAMPAIGN / "launch.py"),
        str(EXPECTED_RUN_CAMPAIGN / "FROZEN_SHA256SUMS"),
        str(EXPECTED_RUN_CAMPAIGN),
        *arguments,
    ]


def worker_environment() -> dict[str, str]:
    home = str(STATE_ROOT.parents[1])
    xdg_runtime_dir = f"/run/user/{os.getuid()}"
    if not Path(xdg_runtime_dir).is_dir():
        raise RuntimeError(
            f"required rootless-container runtime directory is absent: {xdg_runtime_dir}"
        )
    return {
        "HOME": home,
        "PATH": FIXED_WORKER_PATH,
        "PYTHONUNBUFFERED": "1",
        "DEV_HERMIT_PARENT": str(STATE_ROOT),
        "DEV_HERMIT_TOOL_ROOT": str(TOOL_ROOT),
        "RUSTUP_TOOLCHAIN": "nightly",
        "XDG_RUNTIME_DIR": xdg_runtime_dir,
    }


def run_validate_lock_preflight() -> None:
    command = [
        "/usr/bin/with-proxy",
        str(TOOL_ROOT / "ci-hub/ci-hub"),
        "validate-lock",
        "--help",
    ]
    environment = worker_environment()
    process = subprocess.Popen(
        command,
        cwd=CHECKOUT,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=1_800)
    except subprocess.TimeoutExpired as error:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate()
        raise RuntimeError("validate-lock preflight exceeded 1800 seconds") from error
    if process.returncode != 0 or "validate-lock" not in (stdout + stderr):
        detail = stderr.strip() or stdout.strip() or f"exit {process.returncode}"
        raise RuntimeError(f"validate-lock preflight failed: {detail}")


def systemd_command(
    *,
    launcher_sha256: str,
    freeze_manifest_sha256: str,
    freeze_manifest_source: bytes,
    launcher_source: bytes,
    runtime_capsule: str,
) -> list[str]:
    environment = worker_environment()
    systemd_run = Path("/usr/bin/systemd-run")
    with_proxy = Path("/usr/bin/with-proxy")
    if not systemd_run.is_file():
        raise RuntimeError("/usr/bin/systemd-run is unavailable")
    if not with_proxy.is_file():
        raise RuntimeError("/usr/bin/with-proxy is unavailable")
    if "%" in WORKER_BOOTSTRAP:
        raise RuntimeError("worker bootstrap contains an unsafe systemd specifier byte")
    command = [
        str(systemd_run),
        "--user",
        "--quiet",
        "--collect",
        "--expand-environment=no",
        f"--unit={UNIT}",
        "--description=qualify the exact 119-cell KVM complement at 086d41a2",
        f"--property=RuntimeMaxSec={UNIT_RUNTIME_SECONDS}",
        f"--property=LimitFSIZE={PROCESS_FILE_LIMIT_BYTES}",
        f"--property=StandardOutput=append:{LOG}",
        f"--property=StandardError=append:{LOG}",
        f"--working-directory={CHECKOUT}",
        *(f"--setenv={name}={value}" for name, value in environment.items()),
        "--",
        "/usr/bin/env",
        "-i",
        *(f"{name}={value}" for name, value in environment.items()),
        str(with_proxy),
        "/usr/bin/python3",
        "-I",
        "-S",
        "-c",
        WORKER_BOOTSTRAP,
        str(RECORD),
        f"{UNIT}.service",
        TARGET,
        launcher_capsule(launcher_source),
        launcher_sha256,
        freeze_manifest_sha256,
        base64.b64encode(freeze_manifest_source).decode("ascii"),
        str(CAMPAIGN / "launch.py"),
        str(CAMPAIGN),
        "_run",
        "--record",
        str(RECORD),
        "--launcher-sha256",
        launcher_sha256,
        "--freeze-manifest-sha256",
        freeze_manifest_sha256,
        "--runtime-capsule",
        runtime_capsule,
        "--runtime-capsule-sha256",
        runtime_capsule_sha256(runtime_capsule),
    ]
    command_argument_bytes = [len(os.fsencode(argument)) for argument in command]
    per_argument_limit = 32 * os.sysconf("SC_PAGE_SIZE") - 1
    oversized_arguments = [
        index
        for index, size in enumerate(command_argument_bytes)
        if size > per_argument_limit
    ]
    if oversized_arguments:
        raise RuntimeError(
            "frozen worker capsule has an argument above Linux MAX_ARG_STRLEN: "
            + ",".join(str(index) for index in oversized_arguments)
        )
    command_bytes = sum(size + 1 for size in command_argument_bytes)
    inherited_environment_bytes = sum(
        len(os.fsencode(name)) + len(os.fsencode(value)) + 2
        for name, value in os.environ.items()
    )
    pointer_bytes = (len(command) + len(os.environ) + 2) * struct.calcsize("P")
    arg_max = os.sysconf("SC_ARG_MAX")
    capsule_limit = min(arg_max // 2, 512 * 1024)
    if (
        command_bytes >= capsule_limit
        or command_bytes + inherited_environment_bytes + pointer_bytes >= arg_max
    ):
        raise RuntimeError(
            f"frozen worker capsule command is too large: {command_bytes} bytes "
            f"(limit {capsule_limit}, environment {inherited_environment_bytes}, "
            f"pointer table {pointer_bytes}, ARG_MAX {arg_max})"
        )
    return command


def print_paths() -> None:
    print(f"HANDLE {UNIT}.service")
    print(f"RECORD {RECORD}")
    print(f"LOG {LOG}")
    print(f"RESULTS {RESULTS}")
    print(f"CHILD_DEADLINE_SECONDS {CHILD_DEADLINE_SECONDS}")
    print(f"UNIT_RUNTIME_SECONDS {UNIT_RUNTIME_SECONDS}")
    print(f"DEADLINE_BASIS {DEADLINE_BASIS}")


def refuse_unstarted(detail: str, exit_code: int) -> None:
    run_registry.update_record(
        RECORD,
        state="refused",
        result="systemd-launch-refused",
        exit_code=exit_code,
        detail=detail,
        finished_at=utc_now(),
    )


def terminal_fields(exit_code: int, detail: str | None = None) -> dict[str, object]:
    if exit_code == 0:
        fields: dict[str, object] = {
            "state": "completed",
            "result": "passed",
            "exit_code": 0,
            "detail": "campaign evidence complete; qualification is recorded in strict-validation.json",
            "finished_at": utc_now(),
            "results": str(RESULTS),
        }
    else:
        fields = {
            "state": "failed",
            "result": "failed",
            "exit_code": exit_code,
            "detail": detail or "campaign evidence invalid or incomplete",
            "finished_at": utc_now(),
            "results": str(RESULTS),
        }
    # Keep this close to the producer: an invalid terminal vocabulary must be
    # refused before it can strand the durable bench record in a live state.
    run_registry.parse_current_record(initial_record() | fields)
    return fields


def read_successful_qualification() -> dict[str, object]:
    path = RESULTS / "strict-validation.json"
    try:
        document = json.loads(read_unaliased_regular_file(path))
    except Exception as error:
        raise RuntimeError(
            f"payload exited zero without readable strict-validation.json: {error}"
        ) from error
    qualified_ids = document.get("qualified_ids")
    qualified_count = document.get("qualified_cell_count")
    checks = document.get("checks")
    if not (
        document.get("schema") == 2
        and document.get("ok") is True
        and isinstance(checks, dict)
        and checks
        and all(value is True for value in checks.values())
        and isinstance(qualified_ids, list)
        and all(isinstance(value, str) and value for value in qualified_ids)
        and len(set(qualified_ids)) == len(qualified_ids)
        and isinstance(qualified_count, int)
        and not isinstance(qualified_count, bool)
        and 0 <= qualified_count <= 119
        and len(qualified_ids) == qualified_count
        and document.get("qualified_fraction") == f"{qualified_count}/119"
        and document.get("projected_overlap_numerator") == 221 + qualified_count
        and document.get("projected_overlap_denominator") == 340
        and document.get("target_new_cells_required") == 85
        and document.get("target_90_percent_reached") is (qualified_count >= 85)
    ):
        raise RuntimeError(
            "payload exited zero without the exact successful qualification contract"
        )
    return document


def require_final_resource_headroom() -> dict[str, int]:
    logical_bytes = 0
    allocated_bytes = 0
    pending = [RESULTS]
    while pending:
        directory = pending.pop()
        with os.scandir(directory) as entries:
            for entry in entries:
                metadata = entry.stat(follow_symlinks=False)
                if stat.S_ISDIR(metadata.st_mode):
                    pending.append(Path(entry.path))
                elif stat.S_ISREG(metadata.st_mode):
                    logical_bytes += metadata.st_size
                    allocated_bytes += metadata.st_blocks * 512
                else:
                    raise RuntimeError(
                        f"retained evidence contains a non-file node: {entry.path}"
                    )
    filesystem = os.statvfs(RESULTS)
    free_bytes = filesystem.f_bavail * filesystem.f_frsize
    service_metadata = LOG.stat(follow_symlinks=False)
    if (
        not stat.S_ISREG(service_metadata.st_mode)
        or service_metadata.st_nlink != 1
        or service_metadata.st_size >= SERVICE_LOG_MAX_BYTES
    ):
        raise RuntimeError("attributed service log violates its 1 GiB bound")
    if logical_bytes > CAMPAIGN_BUDGET_BYTES or allocated_bytes > CAMPAIGN_BUDGET_BYTES:
        raise RuntimeError("campaign evidence exceeds its 128 GiB aggregate budget")
    if free_bytes < FILESYSTEM_RESERVE_BYTES:
        raise RuntimeError("filesystem free space fell below the 128 GiB reserve")
    return {
        "logical_bytes": logical_bytes,
        "allocated_bytes": allocated_bytes,
        "filesystem_free_bytes": free_bytes,
        "service_log_bytes": service_metadata.st_size,
    }


def emit_final_resource_gate(final_resources: dict[str, int]) -> None:
    line = (
        "WORKER_FINAL_RESOURCE_GATE "
        + json.dumps(final_resources, sort_keys=True, separators=(",", ":"))
    )
    encoded = (line + "\n").encode("utf-8")
    before = LOG.stat(follow_symlinks=False)
    if (
        not stat.S_ISREG(before.st_mode)
        or before.st_nlink != 1
        or before.st_size != final_resources.get("service_log_bytes")
        or before.st_size + len(encoded) >= SERVICE_LOG_MAX_BYTES
    ):
        raise RuntimeError(
            "attributed service log lacks headroom for its final resource gate"
        )
    print(line, flush=True)
    post_resources = require_final_resource_headroom()
    after = LOG.stat(follow_symlinks=False)
    if (
        not stat.S_ISREG(after.st_mode)
        or after.st_nlink != 1
        or (after.st_dev, after.st_ino) != (before.st_dev, before.st_ino)
        or after.st_size >= SERVICE_LOG_MAX_BYTES
        or post_resources.get("service_log_bytes") != after.st_size
    ):
        raise RuntimeError(
            "terminal resource state changed while publishing the final gate"
        )


def parse_freeze_manifest(content: bytes) -> dict[str, str]:
    try:
        text = content.decode("utf-8")
    except UnicodeDecodeError as error:
        raise RuntimeError(f"freeze manifest is not UTF-8: {error}") from error
    observed: dict[str, str] = {}
    for line_number, line in enumerate(text.splitlines(), start=1):
        digest, separator, name = line.partition("  ")
        if (
            separator != "  "
            or len(digest) != 64
            or any(character not in "0123456789abcdef" for character in digest)
            or not name
            or name in observed
        ):
            raise RuntimeError(f"malformed freeze manifest line {line_number}")
        observed[name] = digest
    return observed


def check_freeze_manifest(
    runtime_paths: dict[str, Path] | None = None,
    launcher_sha256: str | None = None,
    freeze_manifest_sha256: str | None = None,
) -> str:
    path = (
        runtime_paths[FREEZE_MANIFEST.name]
        if runtime_paths is not None
        else FREEZE_MANIFEST
    )
    try:
        content = path.read_bytes()
    except OSError as error:
        raise RuntimeError(f"freeze manifest is unreadable: {path}: {error}") from error
    observed_sha256 = hashlib.sha256(content).hexdigest()
    if (
        freeze_manifest_sha256 is not None
        and observed_sha256 != freeze_manifest_sha256
    ):
        raise RuntimeError(
            "freeze manifest bytes disagree with the external trust anchor: "
            f"expected {freeze_manifest_sha256}, got {observed_sha256}"
        )
    observed = parse_freeze_manifest(content)
    expected = FROZEN_HASHES | {
        "launch.py": launcher_sha256 or sha256(Path(__file__).resolve())
    }
    if observed != expected:
        raise RuntimeError("freeze manifest does not exactly bind launcher and payload inputs")
    for name, expected_hash in expected.items():
        path = (
            runtime_paths[name]
            if runtime_paths is not None and name in runtime_paths
            else CAMPAIGN / name
        )
        actual = sha256(path)
        if actual != expected_hash:
            raise RuntimeError(
                f"freeze manifest mismatch for {name}: expected {expected_hash}, got {actual}"
            )
    return observed_sha256


def check_inputs(
    runtime_paths: dict[str, Path] | None = None,
    pass_fds: tuple[int, ...] = (),
    launcher_sha256: str | None = None,
    freeze_manifest_sha256: str | None = None,
) -> dict[str, object]:
    if not CHECKOUT.is_dir():
        raise RuntimeError(f"checkout is absent: {CHECKOUT}")
    check_freeze_manifest(
        runtime_paths,
        launcher_sha256,
        freeze_manifest_sha256,
    )
    for name, expected in FROZEN_HASHES.items():
        path = (
            runtime_paths[name]
            if runtime_paths is not None and name in runtime_paths
            else CAMPAIGN / name
        )
        if not path.is_file():
            raise RuntimeError(f"frozen input is absent: {path}")
        actual = sha256(path)
        if actual != expected:
            raise RuntimeError(
                f"frozen input changed: {name}: expected {expected}, got {actual}"
            )
    head = subprocess.run(
        ["git", "-C", str(CHECKOUT), "rev-parse", "HEAD^{commit}"],
        capture_output=True,
        text=True,
        check=False,
    )
    observed_head = head.stdout.strip()
    if head.returncode != 0 or observed_head != TARGET:
        detail = head.stderr.strip() or observed_head or f"exit {head.returncode}"
        raise RuntimeError(f"checkout is not at exact target {TARGET}: {detail}")
    tree = subprocess.run(
        ["git", "-C", str(CHECKOUT), "rev-parse", "HEAD^{tree}"],
        capture_output=True,
        text=True,
        check=False,
    )
    observed_tree = tree.stdout.strip()
    if tree.returncode != 0 or observed_tree != TARGET_TREE:
        detail = tree.stderr.strip() or observed_tree or f"exit {tree.returncode}"
        raise RuntimeError(f"checkout is not at exact tree {TARGET_TREE}: {detail}")
    status = subprocess.run(
        [
            "git",
            "-C",
            str(CHECKOUT),
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if status.returncode != 0 or status.stdout:
        detail = status.stderr.strip() or status.stdout.strip() or f"exit {status.returncode}"
        raise RuntimeError(f"checkout is not clean: {detail}")
    payload = (
        runtime_paths[PAYLOAD.name]
        if runtime_paths is not None
        else PAYLOAD
    )
    environment = os.environ.copy()
    if runtime_paths is not None:
        environment.update(runtime_input_environment(runtime_paths))
        environment["KVM_RATCHET_PAYLOAD_SHA256"] = FROZEN_HASHES[PAYLOAD.name]
    static = subprocess.run(
        ["/bin/bash", str(payload), "--static-check"],
        cwd=CHECKOUT,
        env=environment,
        capture_output=True,
        text=True,
        check=False,
        pass_fds=pass_fds,
    )
    if static.returncode != 0:
        detail = static.stderr.strip() or static.stdout.strip() or f"exit {static.returncode}"
        raise RuntimeError(f"static population check failed: {detail}")
    try:
        document = json.loads(static.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"static population check emitted invalid JSON: {error}") from error
    if document.get("ok") is not True or document.get("mode") != (
        "static-only-no-build-no-hermit-no-kvm-no-systemd"
    ):
        raise RuntimeError("static population check did not return its exact success contract")
    return document


def static_check() -> int:
    launcher_source = read_unaliased_regular_file(
        Path(__file__).resolve(), max_bytes=MAX_LAUNCHER_BYTES
    )
    freeze_source = read_unaliased_regular_file(
        FREEZE_MANIFEST, max_bytes=MAX_FREEZE_MANIFEST_BYTES
    )
    document = check_inputs(
        launcher_sha256=hashlib.sha256(launcher_source).hexdigest(),
        freeze_manifest_sha256=hashlib.sha256(freeze_source).hexdigest(),
    )
    print(json.dumps(document, sort_keys=True, separators=(",", ":")))
    return 0


def launch(*, dry_run: bool) -> int:
    if (
        EXECUTED_LAUNCHER_SOURCE is None
        or EXECUTED_LAUNCHER_SHA256 is None
        or EXECUTED_FREEZE_MANIFEST_SOURCE is None
        or EXECUTED_FREEZE_MANIFEST_SHA256 is None
    ):
        raise RuntimeError(
            "state-changing launch requires the external dual-digest one-open trusted bootstrap"
        )
    if CHECKOUT != EXPECTED_RUN_CHECKOUT or CAMPAIGN != EXPECTED_RUN_CAMPAIGN:
        raise RuntimeError(
            "state-changing launch is bound to "
            f"{EXPECTED_RUN_CAMPAIGN}, got {CAMPAIGN}"
        )
    launcher_source = EXECUTED_LAUNCHER_SOURCE
    launcher_sha256 = hashlib.sha256(launcher_source).hexdigest()
    if launcher_sha256 != EXECUTED_LAUNCHER_SHA256:
        raise RuntimeError("trusted launcher buffer and digest disagree")
    freeze_manifest_source = EXECUTED_FREEZE_MANIFEST_SOURCE
    freeze_manifest_sha256 = hashlib.sha256(freeze_manifest_source).hexdigest()
    if freeze_manifest_sha256 != EXECUTED_FREEZE_MANIFEST_SHA256:
        raise RuntimeError("trusted freeze-manifest buffer and digest disagree")
    captured_inputs = capture_runtime_input_bytes(
        freeze_manifest_source=freeze_manifest_source,
        freeze_manifest_sha256=freeze_manifest_sha256,
    )
    local_snapshot = create_runtime_input_snapshots(
        captured_inputs,
        freeze_manifest_sha256,
    )
    try:
        check_inputs(
            runtime_paths=local_snapshot["paths"],
            pass_fds=local_snapshot["fds"],
            launcher_sha256=launcher_sha256,
            freeze_manifest_sha256=freeze_manifest_sha256,
        )
    finally:
        close_runtime_input_snapshots(local_snapshot)
    record = initial_record()
    run_registry.parse_current_record(record)
    command = systemd_command(
        launcher_sha256=launcher_sha256,
        freeze_manifest_sha256=freeze_manifest_sha256,
        freeze_manifest_source=freeze_manifest_source,
        launcher_source=launcher_source,
        runtime_capsule=encode_runtime_input_capsule(captured_inputs),
    )

    if RECORD.exists():
        raise RuntimeError(f"run record already exists: {RECORD}")
    if LOG.exists():
        raise RuntimeError(f"run log already exists: {LOG}")

    if dry_run:
        print("DRY-RUN: no record, log, validate lock, or systemd unit was created")
        print_paths()
        print(f"COMMAND {shlex.join(command)}")
        return 0

    active = subprocess.run(
        ["systemctl", "--user", "is-active", "--quiet", f"{UNIT}.service"],
        capture_output=True,
        text=True,
        check=False,
    )
    if active.returncode == 0:
        raise RuntimeError(f"systemd unit is already active: {UNIT}.service")
    if active.returncode not in (3, 4):
        detail = active.stderr.strip() or active.stdout.strip() or f"exit {active.returncode}"
        raise RuntimeError(f"cannot establish systemd unit state: {detail}")

    # Exercise the exact validate-lock entry point and worker toolchain before
    # creating either durable run state or a log.  This is a bounded help-only
    # preflight: it can populate build caches but acquires no validation lock
    # and starts no campaign, Hermit, KVM, or systemd work.
    run_validate_lock_preflight()

    run_registry.create_current_record(RECORD, record)
    try:
        run_registry.reserve_log(LOG)
    except Exception as error:
        refuse_unstarted(str(error), 1)
        raise

    try:
        started = subprocess.run(command, capture_output=True, text=True, check=False)
    except Exception as error:
        refuse_unstarted(f"systemd-run could not be invoked: {error}", 1)
        raise
    if started.returncode != 0:
        detail = started.stderr.strip() or started.stdout.strip() or f"exit {started.returncode}"
        refuse_unstarted(detail, started.returncode)
        raise RuntimeError(f"systemd-run refused service: {detail}")

    print("LAUNCHED: attributed adaptive KVM bench run accepted by systemd")
    print_paths()
    return 0


def prepare_worker_snapshot(
    *,
    launcher_sha256: str,
    freeze_manifest_sha256: str,
    runtime_capsule: str,
    runtime_capsule_digest: str,
) -> tuple[dict[str, object], dict[str, object]]:
    if (
        EXECUTED_LAUNCHER_SOURCE is None
        or EXECUTED_LAUNCHER_SHA256 is None
        or EXECUTED_FREEZE_MANIFEST_SOURCE is None
        or EXECUTED_FREEZE_MANIFEST_SHA256 is None
    ):
        raise RuntimeError("worker requires the trusted immutable launcher capsule")
    observed_launcher_sha256 = hashlib.sha256(
        EXECUTED_LAUNCHER_SOURCE
    ).hexdigest()
    if observed_launcher_sha256 != launcher_sha256:
        raise RuntimeError(
            "launcher changed between initial admission and the systemd worker"
        )
    observed_bootstrap_freeze_sha256 = hashlib.sha256(
        EXECUTED_FREEZE_MANIFEST_SOURCE
    ).hexdigest()
    if (
        observed_bootstrap_freeze_sha256 != freeze_manifest_sha256
        or EXECUTED_FREEZE_MANIFEST_SHA256 != freeze_manifest_sha256
    ):
        raise RuntimeError("worker freeze-manifest trust anchor mismatch")
    observed_runtime_capsule_sha256 = runtime_capsule_sha256(runtime_capsule)
    if observed_runtime_capsule_sha256 != runtime_capsule_digest:
        raise RuntimeError(
            "runtime capsule changed between systemd admission and the worker"
        )
    captured_inputs = decode_runtime_input_capsule(
        runtime_capsule,
        freeze_manifest_sha256,
    )
    observed_freeze_manifest_sha256 = hashlib.sha256(
        captured_inputs[FREEZE_MANIFEST.name]
    ).hexdigest()
    if observed_freeze_manifest_sha256 != freeze_manifest_sha256:
        raise RuntimeError(
            "freeze manifest changed between initial admission and the systemd worker"
        )
    if captured_inputs[FREEZE_MANIFEST.name] != EXECUTED_FREEZE_MANIFEST_SOURCE:
        raise RuntimeError("worker freeze-manifest buffers disagree")
    snapshot = create_runtime_input_snapshots(
        captured_inputs,
        freeze_manifest_sha256,
    )
    try:
        document = check_inputs(
            runtime_paths=snapshot["paths"],
            pass_fds=snapshot["fds"],
            launcher_sha256=observed_launcher_sha256,
            freeze_manifest_sha256=freeze_manifest_sha256,
        )
        for name in RUNTIME_INPUT_NAMES:
            observed = sha256(CAMPAIGN / name)
            expected = snapshot["hashes"][name]
            if observed != expected:
                raise RuntimeError(
                    f"runtime input pathname changed after snapshot: {name}: "
                    f"expected {expected}, got {observed}"
                )
    except Exception:
        close_runtime_input_snapshots(snapshot)
        raise
    receipt = {
        "source_sha": document["source_sha"],
        "source_tree": document["source_tree"],
        "launcher_sha256": observed_launcher_sha256,
        "freeze_manifest_sha256": observed_freeze_manifest_sha256,
        "runtime_capsule_sha256": observed_runtime_capsule_sha256,
        "payload_sha256": FROZEN_HASHES[PAYLOAD.name],
        "observed_payload_sha256": snapshot["hashes"][PAYLOAD.name],
        "payload_inputs": FROZEN_HASHES,
        "runtime_input_source": "sealed-memfd",
        "resource_limits": {
            "process_file_limit_bytes": PROCESS_FILE_LIMIT_BYTES,
            "service_log_max_bytes": SERVICE_LOG_MAX_BYTES,
            "campaign_budget_bytes": CAMPAIGN_BUDGET_BYTES,
            "filesystem_reserve_bytes": FILESYSTEM_RESERVE_BYTES,
        },
        "reviewed_input_trust": (
            "external-launch-and-freeze-digests-plus-argv-capsule-and-"
            "post-boundary-sealed-runtime-memfds"
        ),
        "external_measurement_trust": (
            "with-proxy,systemd,python-stdlib,validate-lock,bash,jq,git,"
            "pinned-checkout-and-toolchain"
        ),
        "checked_at": utc_now(),
    }
    return receipt, snapshot


def verify_worker_snapshot(
    *,
    launcher_sha256: str,
    freeze_manifest_sha256: str,
    runtime_capsule: str,
    runtime_capsule_digest: str,
) -> dict[str, object]:
    receipt, snapshot = prepare_worker_snapshot(
        launcher_sha256=launcher_sha256,
        freeze_manifest_sha256=freeze_manifest_sha256,
        runtime_capsule=runtime_capsule,
        runtime_capsule_digest=runtime_capsule_digest,
    )
    close_runtime_input_snapshots(snapshot)
    return receipt


def run_worker(
    record: Path,
    *,
    launcher_sha256: str,
    freeze_manifest_sha256: str,
    runtime_capsule: str,
    runtime_capsule_digest: str,
) -> int:
    if record.resolve() != RECORD.resolve():
        print(f"launcher: refusing unexpected record path: {record}", file=sys.stderr)
        return 2
    if CHECKOUT != EXPECTED_RUN_CHECKOUT or CAMPAIGN != EXPECTED_RUN_CAMPAIGN:
        print(
            f"launcher: refusing worker outside {EXPECTED_RUN_CAMPAIGN}",
            file=sys.stderr,
        )
        return 2
    if (
        EXECUTED_LAUNCHER_SOURCE is None
        or EXECUTED_LAUNCHER_SHA256 is None
        or EXECUTED_FREEZE_MANIFEST_SOURCE is None
        or EXECUTED_FREEZE_MANIFEST_SHA256 is None
    ):
        print("launcher: refusing worker without trusted immutable capsule", file=sys.stderr)
        return 2
    try:
        # The asynchronous worker owns the launching -> running transition.
        # The parent must never perform a late write that can regress a fast
        # terminal worker record back to running.
        run_registry.update_record(RECORD, state="running")
        receipt, snapshot = prepare_worker_snapshot(
            launcher_sha256=launcher_sha256,
            freeze_manifest_sha256=freeze_manifest_sha256,
            runtime_capsule=runtime_capsule,
            runtime_capsule_digest=runtime_capsule_digest,
        )
        try:
            print(
                "WORKER_FROZEN_INPUTS "
                + json.dumps(receipt, sort_keys=True, separators=(",", ":")),
                flush=True,
            )
            print(f"DEADLINE_BASIS {DEADLINE_BASIS}")
            command = worker_command(
                payload_sha256=str(receipt["payload_sha256"]),
                freeze_manifest_sha256=str(receipt["freeze_manifest_sha256"]),
                runtime_capsule=runtime_capsule,
                runtime_capsule_digest=runtime_capsule_digest,
            )
        finally:
            # These snapshots exist only to run the worker's local admission
            # checks.  validate-lock is cost-wrapped through a subprocess that
            # intentionally closes inherited descriptors, so no pre-boundary
            # descriptor is part of the payload transport contract.
            close_runtime_input_snapshots(snapshot)
        completed = subprocess.run(
            command,
            cwd=CHECKOUT,
            check=False,
        )
        exit_code = completed.returncode
        qualification = None
        final_resources = None
        if exit_code == 0:
            qualification = read_successful_qualification()
            final_resources = require_final_resource_headroom()
            emit_final_resource_gate(final_resources)
        fields = terminal_fields(
            exit_code,
            f"campaign evidence invalid or incomplete: validate-lock or payload exited {exit_code}",
        )
        if qualification is not None:
            fields["detail"] = (
                "campaign evidence complete; same-backend KVM canonical L2 "
                f"qualified {qualification['qualified_cell_count']}/119; "
                f"projected selected-set overlap "
                f"{qualification['projected_overlap_numerator']}/340"
            )
        run_registry.update_record(RECORD, **fields)
        return exit_code
    except Exception as error:
        try:
            run_registry.update_record(RECORD, **terminal_fields(1, f"campaign evidence invalid: {error}"))
        except Exception as record_error:
            print(
                f"launcher: {error}; terminal run-record update also failed: {record_error}",
                file=sys.stderr,
            )
            return 1
        print(f"launcher: {error}", file=sys.stderr)
        return 1


def status() -> int:
    print_paths()
    if RECORD.exists():
        value = run_registry.read_record(RECORD)
        print("RUN_RECORD " + json.dumps(value, sort_keys=True, separators=(",", ":")))
    else:
        print("RUN_RECORD absent")
    observed = subprocess.run(
        [
            "systemctl",
            "--user",
            "show",
            f"{UNIT}.service",
            "--property=LoadState",
            "--property=ActiveState",
            "--property=SubState",
            "--property=Result",
            "--property=ExecMainCode",
            "--property=ExecMainStatus",
            "--no-pager",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    output = observed.stdout.strip()
    print("SYSTEMD " + (output.replace("\n", " ") if output else f"unavailable rc={observed.returncode}"))
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    launch_parser = commands.add_parser("launch")
    launch_parser.add_argument("--dry-run", action="store_true")
    worker = commands.add_parser("_run")
    worker.add_argument("--record", type=Path, required=True)
    worker.add_argument("--launcher-sha256", required=True)
    worker.add_argument("--freeze-manifest-sha256", required=True)
    worker.add_argument("--runtime-capsule", required=True)
    worker.add_argument("--runtime-capsule-sha256", required=True)
    commands.add_parser("status")
    commands.add_parser("static-check")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.command == "launch":
            return launch(dry_run=args.dry_run)
        if args.command == "_run":
            return run_worker(
                args.record,
                launcher_sha256=args.launcher_sha256,
                freeze_manifest_sha256=args.freeze_manifest_sha256,
                runtime_capsule=args.runtime_capsule,
                runtime_capsule_digest=args.runtime_capsule_sha256,
            )
        if args.command == "status":
            return status()
        if args.command == "static-check":
            return static_check()
        raise AssertionError(args.command)
    except RuntimeError as error:
        print(f"launcher: REFUSED: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
