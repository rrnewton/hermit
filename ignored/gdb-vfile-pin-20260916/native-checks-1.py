import os,sys,json,time,subprocess,hashlib,datetime
from pathlib import Path
S=Path(__file__).resolve().parents[2]; E=S/"ignored/gdb-vfile-pin-20260916/native-checks-1"; E.mkdir(exist_ok=False)
os.chdir(S)
def sha(b):return hashlib.sha256(b).hexdigest()
def source():
 paths=subprocess.check_output(["git","ls-files","-z"]).decode().split("\0");files={}
 for name in paths:
  if not name:continue
  q=S/name
  if q.is_symlink():files[name]=sha(os.readlink(q).encode())
  elif q.is_file():files[name]=sha(q.read_bytes())
  else:files[name]=subprocess.check_output(["git","-C",str(q),"rev-parse","HEAD"],text=True).strip()
 return {"head":subprocess.check_output(["git","rev-parse","HEAD"],text=True).strip(),"diff_sha256":sha(subprocess.check_output(["git","diff","--binary","--full-index"])),"files":files}
before=source();(E/"source-before.json").write_text(json.dumps(before,indent=2)+"\n")
pid=os.getpid();stat=Path(f"/proc/{pid}/stat").read_text();cg=Path("/sys/fs/cgroup")/Path(f"/proc/{pid}/cgroup").read_text().strip().split("::",1)[1].lstrip("/");identity={"pid":pid,"start":stat.rsplit(")",1)[1].split()[19],"cgroup":str(cg),"inode":cg.stat().st_ino};(E/"identity.json").write_text(json.dumps(identity,indent=2)+"\n")
env={**os.environ,"CARGO_HOME":str(S/"ignored/gdb-vfile-pin-20260916/cargo"),"CARGO_TARGET_DIR":str(S/"target"),"CARGO_BUILD_JOBS":"4","CARGO_HTTP_CAINFO":"/etc/pki/tls/certs/fb_certs.pem","XDG_CACHE_HOME":str(S/"ignored/gdb-vfile-pin-20260916/host-cache")}
commands=[("pin-policy",["./ci/run-reverie-pin-check.sh","--repo",str(S),"--base-ref","4e7c0636ff105f333d81fa6560069a267e8d60a8"]),("workspace-check",["cargo","check","--locked","--workspace","--all-targets"]),("default-warnings",["./scripts/check-default-build-warnings.sh"]),("fmt",["cargo","fmt","--all","--","--check"]),("shell-syntax",["bash","-n","ci/configure-build-jobs.sh","ci/run-with-reverie-dbt-budget.sh"]),("whitespace",["git","diff","--check"])]
rows=[];status=0;t0=time.monotonic()
try:
 for name,argv in commands:
  t=time.monotonic()
  with (E/(name+".stdout")).open("wb") as out,(E/(name+".stderr")).open("wb") as err:r=subprocess.run(argv,env=env,stdout=out,stderr=err)
  row={"name":name,"argv":argv,"actual_exit":r.returncode,"seconds":time.monotonic()-t};rows.append(row);(E/"commands.json").write_text(json.dumps(rows,indent=2)+"\n");print(json.dumps(row),flush=True)
  if r.returncode:status=r.returncode;break
finally:
 after=source();(E/"source-after.json").write_text(json.dumps(after,indent=2)+"\n")
 resources={}
 for name in ["memory.max","memory.swap.max","memory.peak","memory.events","pids.max","pids.peak","pids.events","cpu.max","cpu.stat"]:
  try:resources[name]=(cg/name).read_text()
  except OSError as ex:resources[name]={"error":str(ex)}
 (E/"resources-final.json").write_text(json.dumps(resources,indent=2)+"\n")
 equal=before==after
 (E/"result.json").write_text(json.dumps({"actual_exit":status,"seconds":time.monotonic()-t0,"source_equal":equal,"commands":rows},indent=2)+"\n")
 if not equal:raise RuntimeError("source changed during checks")
sys.exit(status)
