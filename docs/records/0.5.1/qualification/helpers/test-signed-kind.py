"""Two-node kind qualification of a signed archive in private M1 VM state."""
import argparse,json,os
from pathlib import Path
import subprocess as sp
import tempfile

p=argparse.ArgumentParser();p.add_argument('--archive',type=Path,required=True);p.add_argument('--out',type=Path,required=True);a=p.parse_args()
root=Path.cwd();out=a.out.resolve();out.mkdir(parents=True,exist_ok=False)
env={k:v for k,v in os.environ.items() if not k.startswith('LIGHTER_') and k not in ('DOCKER_HOST','DOCKER_CONTEXT','DOCKER_CONFIG')}
with tempfile.TemporaryDirectory(prefix='lighter-051-kind-',dir='/private/tmp') as directory,(out/'driver.log').open('w') as log:
 work=Path(directory);home=work/'home';home.mkdir()
 sp.run(['tar','-xzf',str(a.archive.resolve()),'-C',work],check=True)
 release=work/'lighter-0.5.1';cli=release/'bin/lighter'
 env.update(LIGHTER_HOME=str(home),LIGHTER_GUEST_DIR=str(release/'share/lighter'),DOCKER_HOST=f'unix://{home}/docker.sock',DOCKER_CONFIG=str(work/'docker-config'))
 (home/'config.json').write_text(json.dumps(dict(cpus=4,memory_mib=4096,disk_gib=64,shares=[str(root)],publish='localhost'))+'\n')
 try:
  sp.run([cli,'start','--timeout','120'],env=env,stdout=log,stderr=sp.STDOUT,check=True,timeout=140)
  sp.run(['python3','scripts/test-kind.py','--expected-version','0.5.1','--out',str(out/'checks'),'--lighter',str(cli),'--kind',str(root/'.logs/kind-m1/bin/kind'),'--kubectl',str(root/'.logs/050/tools/kubectl-1.37.0/kubectl'),'--helm',str(root/'.logs/050/tools/helm'),'--image','kindest/node:v1.37.0@sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5','--base-image','python@sha256:7415fbc3c9e4979cc717d92377ab2bc7b2b4a2af1ac03cc52b5f3f88efedaf3a','--nodes','2'],env=env,check=True,timeout=1800)
  (out/'exit-code').write_text('0\n')
 finally:
  sp.run([cli,'stop'],env=env,stdout=log,stderr=sp.STDOUT,timeout=60)
  sp.run(['python3','scripts/records/unregister-test-bundles.py',str(work)],stdout=log,stderr=sp.STDOUT,timeout=60)
