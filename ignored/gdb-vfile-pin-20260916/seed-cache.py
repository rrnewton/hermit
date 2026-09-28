import hashlib,json,os,subprocess,time
from pathlib import Path
e=Path(__file__).resolve().parent
donor=Path('/home/newton/work/dev-hermit/worktrees/slots/dev-hermit-gdb-replay-diagnostic-20260916/ignored/gdb-replay-diagnostic-20260916/private-cache/cargo')
def tree(p):
 result={}
 for root,ds,fs in os.walk(p):
  for name in sorted(fs):
   q=Path(root)/name;st=q.lstat()
   result[str(q.relative_to(p))]={'size':st.st_size,'inode':st.st_ino,'device':st.st_dev,'mtime_ns':st.st_mtime_ns,'mode':st.st_mode,'sha256':hashlib.sha256(q.read_bytes()).hexdigest() if q.is_file() and not q.is_symlink() else None,'link':os.readlink(q) if q.is_symlink() else None}
 return result
rows=[]
for relative in ['registry/cache','registry/index','git']:
 src=donor/relative;dst=e/'cargo'/relative;dst.parent.mkdir(parents=True,exist_ok=True);assert not dst.exists()
 before=tree(src);t=time.monotonic();p=subprocess.run(['cp','-a','--reflink=auto',str(src),str(dst)]);assert p.returncode==0
 after=tree(src);copied=tree(dst)
 assert before==after and before.keys()==copied.keys()
 for name,v in before.items():
  c=copied[name];assert all(v[k]==c[k] for k in ['size','mode','sha256','link'])
  assert (v['device'],v['inode'])!=(c['device'],c['inode'])
 tag=relative.replace('/','-')
 (e/(tag+'-seed-manifest.json')).write_text(json.dumps({'donor':str(src),'destination':str(dst),'before':before,'after':after,'private':copied},sort_keys=True))
 rows.append({'source':str(src),'destination':str(dst),'actual_exit':0,'seconds':time.monotonic()-t,'entries':len(before),'bytes':sum(v['size'] for v in before.values()),'unchanged_donor':True,'independent_inodes':True,'copied_bytes_equal':True})
(e/'cache-seed.json').write_text(json.dumps(rows,indent=2)+'\n');print(json.dumps(rows),flush=True)
