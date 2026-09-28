#!/usr/bin/python3
"""Run the official DAG generator without modifying tracked input source."""
from pathlib import Path
import hashlib
import json
import os
import runpy
import subprocess
HERE=Path(__file__).resolve().parent
PLAN_SHA256='d11bbf553f351bbec9ade30184b83f442b3c8d5ea5bd3b7f64010efd183d35b8'

def main():
 raw=(HERE/'plan.json').read_bytes()
 assert hashlib.sha256(raw).hexdigest()==PLAN_SHA256
 plan=json.loads(raw)
 helper=Path(plan['helpers']['path'])
 assert hashlib.sha256(helper.read_bytes()).hexdigest()==plan['helpers']['sha256']
 f=runpy.run_path(str(helper),run_name='reviewed_helpers_only')
 require,digest,read_bounded,write_new=(f[k] for k in ['require','digest','read_bounded','write_new'])
 def check_inputs():
  for repo in plan['repositories']:
   f['check_inputs'](dict(repo,inputs=plan['inputs'],optional_cargo_configs=plan['optional_cargo_configs']))
 require(plan['execution']==['/usr/bin/python3','-B',str(HERE/'launch.py')],'wrong caller')
 require([s['name'] for s in plan['stages']]==['generate','check'],'wrong stages')
 for s in plan['stages']:
  require(s['payload']==s['argv'][s['argv'].index('--log-bytes')+2:],'payload/argv mismatch')
 root=Path(plan['run_root']);target=Path(plan['target_dir']);generated=Path(plan['generated_path'])
 require(plan['reuse_owned_target_cache'] is True and target.is_dir() and not target.is_symlink(),'missing owned cache')
 require(target.stat().st_uid==os.getuid() and target.resolve(strict=True).is_relative_to(Path(plan['repositories'][0]['source_root'])/'target'),'wrong cache owner/path')
 for p in [root,Path(plan['observer_root']),generated]:
  require(not p.exists() and not p.is_symlink(),'retain existing output: '+str(p))
 require(generated.parent==root,'generated output outside run')
 check_inputs();root.mkdir(mode=0o700);Path(plan['tmpdir']).mkdir(mode=0o700)
 env={k:os.environ[k] for k in plan['environment_keys'] if k in os.environ};env.update(plan['environment_fixed'])
 write_new(root/'launch.json',dict(plan_sha256=PLAN_SHA256,caller_sha256=digest(__file__),execution=plan['execution'],cwd=str(Path.cwd()),environment=env,repositories=plan['repositories'],scope=plan['scope']))
 records=[];generated_identity=None;active=None
 try:
  for s in plan['stages']:
   active=s['name'];check_inputs()
   if generated_identity:require(digest(generated)==generated_identity['sha256'],'generated file changed before check')
   write_new(root/(active+'-dispatch.json'),dict(argv=s['argv'],cwd=s['cwd']))
   with (root/(active+'-observer.stdout')).open('xb') as stdout,(root/(active+'-observer.stderr')).open('xb') as stderr:
    proc=subprocess.run(s['argv'],cwd=s['cwd'],env=env,stdin=subprocess.DEVNULL,stdout=stdout,stderr=stderr)
   result_path=Path(s['out'])/'result.json';result=json.loads(read_bounded(result_path,1024**2))
   record=dict(stage=active,observer_exit=proc.returncode,result_path=str(result_path),result_sha256=digest(result_path),result=result);records.append(record);write_new(root/(active+'-readback.json'),record)
   f['require_terminal'](result,proc.returncode,s,root,env)
   read_bounded(Path(s['out'])/'stdout',s['reader_limit_bytes']);read_bounded(Path(s['out'])/'stderr',s['reader_limit_bytes'])
   check_inputs();data=read_bounded(generated,16*1024**2);json.loads(data)
   current=dict(path=str(generated),bytes=len(data),sha256=hashlib.sha256(data).hexdigest())
   if generated_identity:require(current==generated_identity,'freshness check changed output')
   else:generated_identity=current;write_new(root/'generated-identity.json',current)
 except Exception as error:
  write_new(root/'summary.json',dict(status='failed',active_stage=active,error=str(error),retained_stages=[r['stage'] for r in records],scope=plan['scope']));raise
 write_new(root/'summary.json',dict(status='passed',generated=generated_identity,observed=[dict(stage=r['stage'],exit=r['result']['wrapper_exit_code'],cpu_nsec=r['result']['final_accounting']['cpu_usage_nsec'],wall_seconds=r['result']['elapsed_seconds']) for r in records],scope=plan['scope']))
 print(json.dumps(dict(status='passed',summary=str(root/'summary.json'))))
if __name__=='__main__':main()
