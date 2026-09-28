from pathlib import Path
import os,subprocess,json,time,hashlib,stat,signal
E=Path(__file__).parent;P=json.loads((E/'plan.json').read_text());root=Path('/home/newton/work/dev-hermit');S=Path('/home/newton/work/dev-hermit/worktrees/slots/dev-hermit-gdb-vfile-pin-20260916')
def save(n,v):
 with(E/n).open('x') as f:json.dump(v,f,indent=2);f.write('\n')
def gen(pid):return int(Path(f'/proc/{pid}/stat').read_text().rsplit(') ',1)[1].split()[19])
def snapshot():
 sets={}
 for label,repo,paths in [('parent-ci-hub',root,['ci-hub']),('primary-manifest-plan',root/'hermit',['ci/manifest-plan','Cargo.toml','Cargo.lock','rust-toolchain.toml','.cargo']),('primary-au-rs',root/'hermit/agent-utils',['rs'])]:
  entries={}
  for rec in subprocess.check_output(['git','-C',str(repo),'ls-files','-s','-z','--',*paths]).split(b'\0'):
   if not rec:continue
   meta,name=rec.split(b'\t',1);mode,oid,stage=meta.decode().split();assert stage=='0';p=repo/os.fsdecode(name)
   if mode=='160000':entries[os.fsdecode(name)]={'gitlink':oid,'actual':subprocess.check_output(['git','-C',str(p),'rev-parse','HEAD']).decode().strip()};continue
   data=os.fsencode(os.readlink(p)) if p.is_symlink() else p.read_bytes();st=p.lstat();entries[os.fsdecode(name)]={'index_object':oid,'mode':stat.S_IMODE(st.st_mode),'uid':st.st_uid,'sha256':hashlib.sha256(data).hexdigest()}
  sets[label]={'repo':str(repo),'head':subprocess.check_output(['git','-C',str(repo),'rev-parse','HEAD']).decode().strip(),'entries':entries}
 plugin=root/'.orc/plugins/hermit-dev/index.ts';st=plugin.stat();sets['protected-plugin']={'sha256':hashlib.sha256(plugin.read_bytes()).hexdigest(),'dev':st.st_dev,'ino':st.st_ino,'size':st.st_size,'mode':stat.S_IMODE(st.st_mode),'mtime_ns':st.st_mtime_ns,'ctime_ns':st.st_ctime_ns}
 return sets
assert subprocess.check_output(['git','-C',str(S),'rev-parse','HEAD']).decode().strip()=='7c4df11ca6ae4b92196d04297e6a650d7e10915e'
assert subprocess.check_output(['git','-C',str(S),'status','--porcelain=v1','--untracked-files=no'])==b''
assert json.loads((S/'ignored/gdb-vfile-pin-20260916/cache-preparation-1/result.json').read_text())['actual_exit']==0
cache=Path(P['cache']);assert cache.is_dir() and cache.resolve()==cache
ancestors=[p for p in [cache,*cache.parents] if(p/'Cargo.toml').exists()];assert not ancestors,ancestors
st=cache.stat();assert stat.S_IMODE(st.st_mode)==0o700 and st.st_uid==os.geteuid()
save('cache-readback.json',{'path':str(cache),'dev':st.st_dev,'inode':st.st_ino,'mode':stat.S_IMODE(st.st_mode),'uid':st.st_uid,'cargo_manifest_ancestors':[],'created_for':'own prior normal commit hook; now ordinary parent help preparation','outside_pin_workspace':S not in cache.parents})
before=snapshot();save('source-before.json',before)
for key in ['CARGO_TARGET_DIR','RUSTUP_TOOLCHAIN','RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS','RUSTC_WRAPPER','RUSTC_WORKSPACE_WRAPPER']:
 assert key not in os.environ,key
cargohome=Path(P['environment_updates']['CARGO_HOME']);assert hashlib.sha256((cargohome/'config.toml').read_bytes()).hexdigest()=='e36ce2ec7de39d1ea2276c016bb660190b960cf4ef23dda9414661bc40ef45b3'
env=dict(os.environ,**P['environment_updates'],CARGO_BUILD_JOBS='4',PYTHONDONTWRITEBYTECODE='1')
cg=Path('/sys/fs/cgroup')/Path('/proc/self/cgroup').read_text().split('::',1)[1].strip().lstrip('/')
def resources():return {n:(cg/n).read_text() for n in ['memory.max','memory.peak','memory.events','memory.swap.max','pids.max','pids.peak','pids.events','cpu.max','cpu.stat']}
save('binding.json',{'pid':os.getpid(),'generation':gen(os.getpid()),'cgroup':str(cg),'inode':cg.stat().st_ino,'resources_before':resources(),'parent_tool':str(root/'ci-hub/ci-hub'),'environment_updates':dict(P['environment_updates'],CARGO_BUILD_JOBS='4'),'borrowed_live_cache':False})
t=time.monotonic();p=None
with(E/'help.stdout').open('xb') as out,(E/'help.stderr').open('xb') as err,(E/'process-samples.jsonl').open('x') as samples:
 p=subprocess.Popen(P['argv'],cwd=P['cwd'],env=env,stdout=out,stderr=err);save('command.json',{'argv':P['argv'],'cwd':P['cwd'],'pid':p.pid,'generation':gen(p.pid),'started_ns':time.time_ns()})
 while p.poll() is None and time.monotonic()-t<850:
  rows=[]
  for pid in map(int,(cg/'cgroup.procs').read_text().split()):
   try:
    q=Path(f'/proc/{pid}');a=(q/'stat').read_text().rsplit(') ',1)[1].split();rows.append({'pid':pid,'start':int(a[19]),'ppid':int(a[1]),'exe':os.readlink(q/'exe'),'argv':[x.decode(errors='replace') for x in(q/'cmdline').read_bytes().split(b'\0') if x]})
   except(FileNotFoundError,ProcessLookupError):pass
  samples.write(json.dumps({'observed_ns':time.time_ns(),'processes':rows})+'\n');samples.flush();time.sleep(1)
 if p.poll() is None:raise RuntimeError('preparation exceeded850s; scope900s retains outer bound, no retry or private-cache cleanup')
after=snapshot();save('source-after.json',after);same=before==after
save('result.json',{'actual_exit':p.returncode,'elapsed_seconds':time.monotonic()-t,'source_equal':same,'resources_final':resources(),'head':subprocess.check_output(['git','-C',str(S),'rev-parse','HEAD']).decode().strip(),'status':subprocess.check_output(['git','-C',str(S),'status','--porcelain=v1','--untracked-files=all']).decode(),'cache_note':'Prepared private cache. Real ordinary ci-hub --help only; no validator or admission mutation.'})
assert same,'tool source or protected plugin changed during preparation'
raise SystemExit(p.returncode)
