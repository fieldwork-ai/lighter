from pathlib import Path
import argparse,hashlib,json,os,subprocess as sp,tempfile,time,fcntl,shutil
root=Path.home()/'lighter';source=root/'.logs/051/demand-source';base=root/'.logs/051/demand'
p=argparse.ArgumentParser();p.add_argument('--arm',type=int);p.add_argument('--home',type=Path);p.add_argument('--mode',choices=['background','demand-all']);a=p.parse_args()
cli=source/'target/release/lighter';guest=root/'guest/out';out=base/'m1-cold-memory-a1'
env={k:v for k,v in os.environ.items() if not k.startswith('LIGHTER_') and k not in ('DOCKER_HOST','DOCKER_CONTEXT','DOCKER_CONFIG')}
env['LIGHTER_GUEST_DIR']=str(guest)
program='''const n=2*1024*1024*1024;const t=process.hrtime.bigint();const b=Buffer.allocUnsafe(n);b.fill(0x5a);const allocated=process.hrtime.bigint();for(let i=0;i<n;i+=4096){if(b[i]!==0x5a)throw Error('corrupt '+i);}console.log(JSON.stringify({mib:n/1048576,allocation_ms:Number(allocated-t)/1e6,verify_ms:Number(process.hrtime.bigint()-allocated)/1e6}));'''
if a.arm is not None:
 if owner_file:=os.environ.get('LIGHTER_BENCH_OWNER_FILE'):
  with Path(owner_file).open('a') as owners:owners.write(json.dumps(str(a.home/'lighter.app/Contents/MacOS/lighter'))+'\n')
 env.update(LIGHTER_HOME=str(a.home),DOCKER_HOST='unix://'+str(a.home/'docker.sock'),LIGHTER_DEMAND_RAM='1' if a.mode=='demand-all' else '0',LIGHTER_DEMAND_BASE='1' if a.mode=='demand-all' else '0',LIGHTER_BACKGROUND_RAM='1')
 arm=out/f'{a.arm}-{a.mode}';arm.mkdir();rows=[]
 with (arm/'commands.log').open('w') as log:
  def run(cmd,timeout=180):
   r=sp.run([str(x) for x in cmd],env=env,capture_output=True,text=True,timeout=timeout);log.write(r.stdout+r.stderr);log.flush();r.check_returncode();return r.stdout
  try:
   for rep in range(3):
    start=time.monotonic();run([cli,'start']);start_ms=1000*(time.monotonic()-start)
    allocations=[]
    for turn in range(2):
     allocations.append(json.loads(run(['docker','run','--rm','--memory','3g','lighter-bench:1','node','-e',program])))
     if turn==0:time.sleep(15)
    rows.append(dict(rep=rep,start_ms=start_ms,allocations=allocations));(arm/'results.json').write_text(json.dumps(rows,indent=2)+'\n')
    run([cli,'stop'])
   (arm/'exit-code').write_text('0\n')
  finally:run([cli,'stop'])
 raise SystemExit(0)
out.mkdir(exist_ok=False);home=Path(tempfile.mkdtemp(prefix='lighter-demand-cold-',dir='/private/tmp'));(out/'home').write_text(str(home)+'\n')
(home/'config.json').write_text(json.dumps(dict(cpus=8,memory_mib=16384,disk_gib=64,shares=[],publish='localhost'))+'\n')
env.update(LIGHTER_HOME=str(home),DOCKER_HOST='unix://'+str(home/'docker.sock'),LIGHTER_DEMAND_RAM='0',LIGHTER_DEMAND_BASE='0',LIGHTER_BACKGROUND_RAM='1')
artifacts={str(f):hashlib.file_digest(f.open('rb'),'sha256').hexdigest() for f in [cli,guest/'Image',guest/'rootfs.ext4',Path(__file__),root/'.logs/050/benchmark-images/arm64.tar']}
order=['background','demand-all','demand-all','background'];(out/'environment.json').write_text(json.dumps(dict(artifacts=artifacts,source=sp.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip(),order=order,profile=json.loads((home/'config.json').read_text()),program=program),indent=2)+'\n')
passed=False
try:
 with (out/'setup.log').open('w') as log:
  for cmd in [[cli,'start'],['docker','load','-i',root/'.logs/050/benchmark-images/arm64.tar'],['docker','create','--name','saved-fixture','lighter-bench:1','true'],[cli,'stop']]:sp.run([str(x) for x in cmd],env=env,stdout=log,stderr=sp.STDOUT,check=True,timeout=300)
 for i,mode in enumerate(order,1):
  with (out/f'{i}-{mode}.log').open('w') as log:
   sp.run(['python3',str(source/'benchmarks/guard.py'),'--target','lighter','--quiet','--timeout','900','--log',str(out/f'{i}-{mode}-guard.jsonl'),'--','python3',str(Path(__file__).resolve()),'--arm',str(i),'--mode',mode,'--home',str(home)],cwd=source,env=env,stdout=log,stderr=sp.STDOUT,check=True,timeout=1500)
  print('PASS',i,mode,flush=True)
 for f,expected in artifacts.items():assert hashlib.file_digest(Path(f).open('rb'),'sha256').hexdigest()==expected
 passed=True
finally:
 sp.run([str(cli),'stop'],env=env,capture_output=True,timeout=120)
 sp.run(['python3',str(source/'scripts/records/unregister-test-bundles.py'),str(home)],capture_output=True,timeout=60)
 (out/'exit-code').write_text('0\n' if passed else '1\n')
