from pathlib import Path
import subprocess,hashlib,json,os,time,sys
S=Path(__file__).resolve().parents[2];E=S/"ignored/gdb-vfile-pin-20260916/commit-2";E.mkdir(exist_ok=False);os.chdir(S)
env={**os.environ,"CARGO_HOME":str(S/"ignored/gdb-vfile-pin-20260916/cargo"),"CARGO_TARGET_DIR":str(S/"target"),"CARGO_BUILD_JOBS":"4","XDG_CACHE_HOME":"/home/newton/work/dev-hermit/ignored/ci-hub/run1828-fix-forward-20260916/gdb-vfile-pin-official-1/host-cache","CARGO_HTTP_CAINFO":"/etc/pki/tls/certs/fb_certs.pem"}
def call(name,args):
 t=time.monotonic();q=subprocess.run(args,capture_output=True,env=env);(E/(name+".stdout")).write_bytes(q.stdout);(E/(name+".stderr")).write_bytes(q.stderr);(E/(name+".json")).write_text(json.dumps({"argv":args,"actual_exit":q.returncode,"seconds":time.monotonic()-t},indent=2)+"\n");print(name,q.returncode,flush=True);return q
proof=json.loads((S/"ignored/gdb-vfile-pin-20260916/PRECOMMIT-SOURCE-AND-CHECKS.json").read_text());paths=proof["changed_paths"]
assert subprocess.check_output(["git","rev-parse","HEAD"],text=True).strip()==proof["base"]
assert sorted(subprocess.check_output(["git","diff","--cached","--name-only"],text=True).splitlines())==sorted(paths)
assert hashlib.sha256(subprocess.check_output(["git","diff","--binary","--full-index"])).hexdigest()==proof["patch_sha256"]
q=call("who-am-i",["/home/newton/work/dev-hermit/ci-hub/bin/who-am-i","--tag","--role","impl"]);assert q.returncode==0
tag=q.stdout.decode().strip();assert tag.startswith("[") and tag.endswith("]") and "\n" not in tag
body="Pin Reverie unsupported vFile handling\n\n"+tag+"\n\nPlain Language Summary and Project Impact\n\nAdvance Hermit's uniform Reverie dependency to the landed unsupported-vFile repair from https://github.com/rrnewton/reverie/pull/559. Unknown GDB host-I/O probes now receive the protocol's empty unsupported response so replay can continue to the existing classifier and fault-injection checks. The pin range also includes the preceding landed paused-counter API change; this commit carries no temporary CLI diagnostic edits.\n\nUpdate eight Cargo manifests, both tracked locks and the two coupled CI pin bindings. Carry the existing native DynamoRIO build budget by identical build-script and vendored-source inputs, without changing the 16-job clamp or 1050 effective-job-second threshold. This is not a new timing measurement.\n\nValidation\n\nNormal pin ancestry/uniformity policy, locked whole-workspace all-target check, supported whole-workspace Clippy with warnings denied, workspace fmt, shell syntax and whitespace checks all passed in 113.980 seconds under 4 CPUs and 8 GiB. No Hermit guest result is claimed yet; the unchanged official 78-case CLI selection with 35 existing filters is planned. The upstream repair separately passed all 39 gdbstub tests (138 unrelated tests filtered), with two intended old-source failures. No test assertion, comparison policy, timeout, filter, node budget or verdict classification changes here.\n\nTask: vision-ci-signal-is-trustworthy-end-to-end\n"
(E/"message.txt").write_text(body)
q=call("stage",["git","add","--",*paths]);assert q.returncode==0
assert sorted(subprocess.check_output(["git","diff","--cached","--name-only"],text=True).splitlines())==sorted(paths)
(E/"staged.patch").write_bytes(subprocess.check_output(["git","diff","--cached","--binary","--full-index"]))
q=call("commit",["git","commit","--only","-F",str(E/"message.txt"),"--",*paths])
(E/"status-after.txt").write_bytes(subprocess.check_output(["git","status","--porcelain"]))
if q.returncode:sys.exit(q.returncode)
head=subprocess.check_output(["git","rev-parse","HEAD"],text=True).strip();tree=subprocess.check_output(["git","rev-parse","HEAD^{tree}"],text=True).strip();patch=subprocess.check_output(["git","diff","--binary","--full-index",proof["base"],head]);assert hashlib.sha256(patch).hexdigest()==proof["patch_sha256"]
assert not subprocess.check_output(["git","status","--porcelain","--untracked-files=no"])
(E/"committed.patch").write_bytes(patch);(E/"COMMITTED.json").write_text(json.dumps({"head":head,"tree":tree,"base":proof["base"],"patch_sha256":hashlib.sha256(patch).hexdigest(),"paths":paths,"actual_tag":tag,"body_sha256":hashlib.sha256(body.encode()).hexdigest(),"tracked_clean":True},indent=2)+"\n");print(head,tree,flush=True)
