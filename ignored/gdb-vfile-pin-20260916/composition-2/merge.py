from pathlib import Path
import subprocess, json, os, hashlib, time

E = Path(__file__).resolve().parent
S = E.parents[2]
os.chdir(S)
main = "fe41081742652fce09fc22f20469df10fa09ddaa"
prior = "932f7130ea3c77dcdb2547b6834f46f08bf593f0"
env = {**os.environ, "TG_DB_PATH": "/home/newton/.tg/hermit2.db",
       "CARGO_HOME": str(S / "ignored/gdb-vfile-pin-20260916/cargo"),
       "CARGO_TARGET_DIR": str(S / "target"), "CARGO_BUILD_JOBS": "4",
       "XDG_CACHE_HOME": "/home/newton/work/dev-hermit/ignored/ci-hub/run1828-fix-forward-20260916/gdb-vfile-pin-official-1/host-cache",
       "CARGO_HTTP_CAINFO": "/etc/pki/tls/certs/fb_certs.pem"}

def get(*args):
    return subprocess.check_output(args, env=env)

def call(name, argv):
    t = time.monotonic()
    q = subprocess.run(argv, env=env, capture_output=True)
    (E / (name + ".stdout")).write_bytes(q.stdout)
    (E / (name + ".stderr")).write_bytes(q.stderr)
    (E / (name + ".json")).write_text(json.dumps({"argv": argv, "actual_exit": q.returncode, "seconds": time.monotonic()-t}, indent=2)+"\n")
    print(name, q.returncode, flush=True)
    return q

pid = os.getpid()
generation = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19]
cgroup = Path("/sys/fs/cgroup") / Path(f"/proc/{pid}/cgroup").read_text().strip().split(":", 2)[2].lstrip("/")
identity = {"pid": pid, "start": generation, "cgroup": str(cgroup), "inode": cgroup.stat().st_ino}
(E / "identity.json").write_text(json.dumps(identity, indent=2)+"\n")
q = call("scope-before", ["systemctl", "--user", "show", "hermit-gdb-vfile-pin-compose2-20260917.scope", "--property=InvocationID,ControlGroup,CPUQuotaPerSecUSec,MemoryMax,MemorySwapMax,TasksMax,RuntimeMaxUSec"])
assert q.returncode == 0
assert get("git", "rev-parse", "HEAD").decode().strip() == prior
assert get("git", "branch", "--show-current").decode().strip() == "dev-hermit/gdb-vfile-pin-20260916"
assert not get("git", "status", "--porcelain", "--untracked-files=no")
assert get("git", "rev-parse", "origin/main").decode().strip() == main
handoff = (S / "HANDOFF.md").read_bytes()
assert hashlib.sha256(handoff).hexdigest() == "d079649b74ab74668a334c5739a911ce63b0bcd3ff34e0b8433232e943f8170e"
(E / "HANDOFF-before.md").write_bytes(handoff)
assessment = json.loads((E.parent / "publication-preparation-1/COMPOSITION-ASSESSMENT.json").read_text())
assert assessment["current_main"] == main and assessment["tested_head"] == prior
assert assessment["all12_cli_rows_equal"] and not assessment["overlap"]
q = call("merge-stage", ["git", "merge", "--no-ff", "--no-commit", main])
assert q.returncode == 0
pin_patch = get("git", "diff", "--cached", "--binary", "--full-index", main)
assert hashlib.sha256(pin_patch).hexdigest() == "6e9338d0f661ec1db8a399b494509ab6e9b08a125b601f93937482f6715a88da"
expected = assessment["incoming_paths"]
assert sorted(get("git", "diff", "--cached", "--name-only", prior).decode().splitlines()) == sorted(expected)
for path in expected:
    assert get("git", "show", ":"+path) == get("git", "show", main+":"+path)
q = call("who-am-i", ["/home/newton/work/dev-hermit/ci-hub/bin/who-am-i", "--tag", "--role", "impl"])
assert q.returncode == 0
tag = q.stdout.decode().strip()
assert tag.startswith("[") and tag.endswith("]") and "\n" not in tag
message = "Compose current main with the verified Reverie pin\n\n" + tag + "\n\nPlain Language Summary and Project Impact\n\nBring already-landed current-main E2E qualification and pressure-test changes into the Reverie protocol pin branch without changing its twelve-file pin patch. All eleven incoming files are byte-identical to main fe410817. The existing CLI source, exact twelve-node closure, image, selectors, assertions and limits are unchanged. The native build-budget carry still uses identical recipe inputs and unchanged numerical bounds; it is not a new timing measurement.\n\nThe prior pin head 932f7130 passed all twelve official selected nodes and all 78 CLI tests, including the replay-stage SIGKILL classification regression. The unchanged 113-identity inventory has 34 skipped matches and one existing ignored case. That result remains bound to 932f7130: this later composition has not run another guest selection and does not claim a full-main receipt. Normal commit hooks run on this composition.\n\nUpstream protocol repair: https://github.com/rrnewton/reverie/pull/559\nTask: vision-ci-signal-is-trustworthy-end-to-end\n"
(E / "merge-message.txt").write_text(message)
q = call("merge-commit", ["git", "commit", "-F", str(E / "merge-message.txt")])
assert q.returncode == 0
head = get("git", "rev-parse", "HEAD").decode().strip()
tree = get("git", "rev-parse", "HEAD^{tree}").decode().strip()
assert get("git", "show", "-s", "--format=%P", head).decode().split() == [prior, main]
patch = get("git", "diff", "--binary", "--full-index", main, head)
assert patch == pin_patch
assert not get("git", "status", "--porcelain", "--untracked-files=no")
assert (S / "HANDOFF.md").read_bytes() == handoff
(E / "composed-pin.patch").write_bytes(patch)
(E / "COMPOSED.json").write_text(json.dumps({"head": head, "tree": tree, "main": main, "tested_prior_pin_head": prior, "pin_patch_sha256": hashlib.sha256(patch).hexdigest(), "incoming_eleven_files_exact": True, "tracked_clean": True, "handoff_unchanged": True, "actual_tag": tag, "guest_result_scope": "78 PASS at prior932; no new guest run at composed head"}, indent=2)+"\n")
resources = {}
for name in ["memory.max", "memory.peak", "memory.events", "memory.swap.max", "pids.max", "pids.peak", "pids.events", "cpu.max"]:
    try: resources[name] = (cgroup / name).read_text()
    except OSError as err: resources[name] = {"error": str(err)}
(E / "resources-final.json").write_text(json.dumps(resources, indent=2)+"\n")
print(head, tree, flush=True)
