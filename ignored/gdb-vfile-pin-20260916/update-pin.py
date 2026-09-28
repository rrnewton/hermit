from pathlib import Path
import subprocess,json,os,sys,time,hashlib
s=Path(__file__).resolve().parents[2];e=Path(__file__).resolve().parent
out=e/'update-1';out.mkdir(exist_ok=False)
def git(*a):return subprocess.check_output(['git',*a],cwd=s)
def snapshot():return {'head':git('rev-parse','HEAD').decode().strip(),'diff':git('diff','--binary','--full-index').decode(),'files':{p:hashlib.sha256((s/p).read_bytes()).hexdigest() for p in git('ls-files','-z').decode().split('\0') if p and (s/p).is_file() and not (s/p).is_symlink()}}
def put(name,value):(out/name).write_text(json.dumps(value,indent=2)+'\n')
before=snapshot();put('source-before.json',before);assert before['head']=='4e7c0636ff105f333d81fa6560069a267e8d60a8' and not before['diff']
stat=Path('/proc/self/stat').read_text();cg=Path('/proc/self/cgroup').read_text().split('0::',1)[1].strip();cgpath=Path('/sys/fs/cgroup')/cg.lstrip('/')
put('identity.json',{'pid':os.getpid(),'start':stat[stat.rfind(')')+2:].split()[19],'cgroup':str(cgpath),'inode':cgpath.stat().st_ino})
records=[]
def run(name,argv,env=None):
 t=time.monotonic()
 with (out/(name+'.stdout')).open('wb') as stdout,(out/(name+'.stderr')).open('wb') as stderr:p=subprocess.run(argv,cwd=s,env=env,stdout=stdout,stderr=stderr)
 row={'name':name,'argv':argv,'actual_exit':p.returncode,'seconds':time.monotonic()-t};records.append(row);put('commands.json',records);print(json.dumps(row),flush=True);return p.returncode
rc=run('seed-cache',[sys.executable,str(e/'seed-cache.py')]);assert rc==0
remote=subprocess.run(['git','ls-remote','https://github.com/rrnewton/reverie.git','refs/heads/main'],cwd=s,capture_output=True,text=True);put('remote-main.json',{'actual_exit':remote.returncode,'stdout':remote.stdout,'stderr':remote.stderr});assert remote.returncode==0 and remote.stdout.split()[0]=='d87a03a312421d34dee81dae71aa395b40231e63'
env=os.environ.copy();env.update(CARGO_HOME=str(e/'cargo'),CARGO_TARGET_DIR=str(s/'target'),CARGO_BUILD_JOBS='4',CARGO_HTTP_CAINFO='/etc/pki/tls/certs/fb_certs.pem',XDG_CACHE_HOME=str(e/'host-cache'))
rc=run('normal-update',['./ci/run-reverie-pin-check.sh','--repo',str(s),'--update-to-latest','--base-ref','4e7c0636ff105f333d81fa6560069a267e8d60a8'],env)
put('source-after.json',snapshot());put('resources-final.json',{n:(cgpath/n).read_text() for n in ['memory.max','memory.swap.max','memory.peak','memory.events','pids.max','pids.peak','pids.events','cpu.max','cpu.stat'] if (cgpath/n).exists()});put('result.json',{'actual_exit':rc,'commands':records});sys.exit(rc)
