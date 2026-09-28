from pathlib import Path
import os,stat,subprocess,json,hashlib,time,sys
S=Path(__file__).resolve().parents[2];E=S/"ignored/gdb-vfile-pin-20260916/cache-preparation-1";E.mkdir(exist_ok=False)
D=Path("/home/newton/work/dev-hermit/worktrees/slots/dev-hermit-gdb-replay-diagnostic-20260916");O=S/"ignored/hermetic/split";assert not O.exists();O.mkdir(parents=True)
def save(n,v):(E/n).write_text(json.dumps(v,indent=2)+"\n")
def digest(p):
 with p.open("rb") as f:return hashlib.file_digest(f,"sha256").hexdigest()
def snapshot(root):
 entries={}
 for d,dirs,files in os.walk(root,followlinks=False):
  dirs.sort();files.sort()
  for name in dirs+files:
   q=Path(d)/name;k=str(q.relative_to(root));st=q.lstat();v={"mode":st.st_mode,"dev":st.st_dev,"inode":st.st_ino,"mtime_ns":st.st_mtime_ns,"ctime_ns":st.st_ctime_ns}
   if stat.S_ISLNK(st.st_mode):v.update(kind="symlink",link=os.readlink(q))
   elif stat.S_ISREG(st.st_mode):v.update(kind="file",size=st.st_size,sha256=digest(q))
   elif stat.S_ISDIR(st.st_mode):v.update(kind="directory")
   else:raise RuntimeError("nonordinary cache entry: "+str(q))
   entries[k]=v
 return entries
pid=os.getpid();cg=Path("/sys/fs/cgroup")/Path(f"/proc/{pid}/cgroup").read_text().strip().split("::",1)[1].lstrip("/");save("identity.json",{"pid":pid,"start":Path(f"/proc/{pid}/stat").read_text().rsplit(")",1)[1].split()[19],"cgroup":str(cg),"inode":cg.stat().st_ino})
assert subprocess.check_output(["git","-C",str(D),"rev-parse","HEAD"],text=True).strip()=="3d6d90350715c56e00c52826231ef621d17e8c17"
assert not subprocess.check_output(["git","-C",str(D),"status","--porcelain","--untracked-files=no"])
for unit in ["hermit-gdb-vfile-pin-native-1-20260916.scope","hermit-gdb-vfile-pin-commit-3-20260916.scope"]:
 q=subprocess.run(["systemctl","--user","show",unit,"-p","LoadState","--value"],capture_output=True,text=True);assert q.returncode==0 and q.stdout.strip()=="not-found"
plan=[("debug",D/"ignored/hermetic/split/target/debug",O/"target/debug"),("release",D/"ignored/hermetic/split/target/release",O/"target/release"),("registry",S/"ignored/gdb-vfile-pin-20260916/cargo/registry",O/"cargo/registry"),("git",S/"ignored/gdb-vfile-pin-20260916/cargo/git",O/"cargo/git")]
rows=[];started=time.monotonic();beforefs=os.statvfs(S);status=0
try:
 for label,src,dst in plan:
  assert src.is_dir() and not src.is_symlink() and not dst.exists();t=time.monotonic();pre=snapshot(src);save(label+"-source-before.json",pre);dst.mkdir(parents=True)
  argv=["cp","--reflink=auto","-a","--no-preserve=ownership",str(src)+"/.",str(dst)]
  with (E/(label+"-copy.stdout")).open("wb") as out,(E/(label+"-copy.stderr")).open("wb") as err:q=subprocess.run(argv,stdout=out,stderr=err)
  row={"label":label,"source":str(src),"destination":str(dst),"argv":argv,"actual_copy_exit":q.returncode};rows.append(row);save("copies.json",rows);assert q.returncode==0
  after=snapshot(src);dest=snapshot(dst);save(label+"-source-after.json",after);save(label+"-destination.json",dest);assert pre==after and set(pre)==set(dest)
  count=0;size=0
  for name,v in pre.items():
   w=dest[name];assert v["mode"]==w["mode"] and v["kind"]==w["kind"],name
   if v["kind"]=="file":assert v["sha256"]==w["sha256"] and v["size"]==w["size"] and (v["dev"],v["inode"])!=(w["dev"],w["inode"]);count+=1;size+=v["size"]
   elif v["kind"]=="symlink":assert v["link"]==w["link"]
  row.update(seconds=time.monotonic()-t,files=count,logical_bytes=size,source_before_after_equal=True,no_shared_file_inodes=True);save("copies.json",rows);print(json.dumps(row),flush=True)
 config=Path("/home/newton/.cargo/config.toml");raw=config.read_bytes();assert hashlib.sha256(raw).hexdigest()=="e36ce2ec7de39d1ea2276c016bb660190b960cf4ef23dda9414661bc40ef45b3";(O/"cargo/config.toml").write_bytes(raw)
 assert not (O/"target/ci").exists() and not (O/"target/tmp").exists()
except BaseException as ex:
 status=1;save("failure.json",{"type":type(ex).__name__,"message":str(ex)});raise
finally:
 fs=os.statvfs(S);resources={}
 for name in ["memory.max","memory.swap.max","memory.peak","memory.events","pids.max","pids.peak","pids.events","cpu.max","cpu.stat"]:
  try:resources[name]=(cg/name).read_text()
  except OSError as ex:resources[name]={"error":str(ex)}
 save("resources-final.json",resources);save("result.json",{"actual_exit":status,"seconds":time.monotonic()-started,"copies":rows,"device_used_before":(beforefs.f_blocks-beforefs.f_bfree)*beforefs.f_frsize,"device_used_after":(fs.f_blocks-fs.f_bfree)*fs.f_frsize,"available_after":fs.f_bavail*fs.f_frsize,"scope":"Private compilation caches only. No target/ci prepared authority, target/tmp old test outputs or install_pkg copied. Normal source-bound producers remain required."})
sys.exit(status)
