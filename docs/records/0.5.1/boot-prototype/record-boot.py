#!/usr/bin/env python3
"""Isolated cold-start observations; never operates the default machine home."""
import argparse,hashlib,json,os,subprocess as sp,tempfile,time,statistics
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--cli',required=True);p.add_argument('--guest',required=True);p.add_argument('--out',required=True);p.add_argument('--memory',type=int,nargs='+',required=True);p.add_argument('--reps',type=int,default=10);p.add_argument('--timing',action='store_true');p.add_argument('--background',action='store_true');a=p.parse_args()
out=Path(a.out).resolve();out.mkdir(parents=True,exist_ok=False)
cli=str(Path(a.cli).resolve());guest=str(Path(a.guest).resolve())
env={k:v for k,v in os.environ.items() if not k.startswith('LIGHTER_') and k not in ('DOCKER_HOST','DOCKER_CONTEXT','DOCKER_CONFIG')}
if a.timing:env['LIGHTER_BOOT_TIMING']='1'
if a.background:env['LIGHTER_BACKGROUND_RAM']='1'
env['PATH']=str(Path.home()/'.orbstack/bin')+':/opt/homebrew/bin:'+env['PATH']
def digest(p):return hashlib.sha256(Path(p).read_bytes()).hexdigest()
def run(cmd,**kw):return sp.run(cmd,env=env,check=True,timeout=120,**kw)
metadata={'cli':cli,'cli_sha256':digest(cli),'guest':guest,'payload':{n:digest(Path(guest)/n) for n in ['Image','rootfs.ext4']},'source':sp.check_output(['git','rev-parse','HEAD'],text=True).strip(),'platform':sp.check_output(['sw_vers'],text=True),'cpus':8,'memory_mib':a.memory,'reps':a.reps,'sampling':'docker version poll every 50ms; first Alpine true with image present','shared_host':True,'timing':a.timing,'background':a.background}
(out/'environment.json').write_text(json.dumps(metadata,indent=2)+'\n')
results=[]
try:
 for memory in a.memory:
  home=Path(tempfile.mkdtemp(prefix='lighter-051-boot-',dir='/private/tmp'))
  env.update(LIGHTER_HOME=str(home),LIGHTER_GUEST_DIR=guest,DOCKER_HOST='unix://'+str(home/'docker.sock'))
  log=(out/f'{memory}.log').open('w');dk=['docker','--host',env['DOCKER_HOST']]
  (out/f'{memory}-home').write_text(str(home)+'\n')
  try:
   run([cli,'config','--cpus','8','--memory',str(memory),'--disk','128'],stdout=log,stderr=sp.STDOUT)
   run([cli,'start','--timeout','120'],stdout=log,stderr=sp.STDOUT)
   run(dk+['pull','alpine:3.21'],stdout=log,stderr=sp.STDOUT)
   info=json.loads(run(dk+['image','inspect','alpine:3.21'],capture_output=True,text=True).stdout)[0]
   image=info['Id'];(out/f'{memory}-image.json').write_text(json.dumps({'Id':image,'RepoDigests':info['RepoDigests']},indent=2)+'\n')
   run(dk+['run','--rm',image,'true'],stdout=log,stderr=sp.STDOUT)
   for rep in range(a.reps):
    run([cli,'stop'],stdout=log,stderr=sp.STDOUT);time.sleep(2)
    processes=sp.check_output(['ps','-axo','pid=,%cpu=,rss=,comm='],text=True)
    (out/f'{memory}-{rep+1}-host.txt').write_text(processes)
    t0=time.monotonic_ns();start=sp.Popen([cli,'start','--timeout','120'],env=env,stdout=log,stderr=sp.STDOUT)
    try:
     while True:
      probe=sp.run(dk+['version'],env=env,stdout=sp.DEVNULL,stderr=sp.DEVNULL,timeout=5)
      if probe.returncode==0:break
      if start.poll() is not None and start.returncode!=0:raise RuntimeError('start failed')
      if (time.monotonic_ns()-t0)>120e9:raise TimeoutError('Docker readiness')
      time.sleep(.05)
     t1=time.monotonic_ns();run(dk+['run','--rm',image,'true'],stdout=log,stderr=sp.STDOUT);t2=time.monotonic_ns()
     assert start.wait(timeout=120)==0
    finally:
     if start.poll() is None:start.terminate();start.wait(timeout=10)
    row={'memory_mib':memory,'rep':rep+1,'docker_ms':(t1-t0)/1e6,'first_container_ms':(t2-t0)/1e6,'wall_time':time.time()};results.append(row)
    (out/'results.json').write_text(json.dumps(results,indent=2)+'\n');print(json.dumps(row),flush=True)
  finally:
   sp.run([cli,'stop'],env=env,stdout=log,stderr=sp.STDOUT,timeout=120)
   log.close()
 (out/'exit-code').write_text('0\n')
except BaseException as e:
 (out/'failure.json').write_text(json.dumps({'error':str(e),'type':type(e).__name__})+'\n');(out/'exit-code').write_text('1\n');raise
for memory in a.memory:
 rows=[r for r in results if r['memory_mib']==memory]
 print('MEDIAN',memory,statistics.median(r['docker_ms'] for r in rows),statistics.median(r['first_container_ms'] for r in rows),flush=True)
