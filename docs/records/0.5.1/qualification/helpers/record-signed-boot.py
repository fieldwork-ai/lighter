"""ABBA cold boots of both shipped archives, guarded on the dedicated M1."""
import argparse, hashlib, json, os
from pathlib import Path
import subprocess as sp

p=argparse.ArgumentParser();p.add_argument('--old',type=Path,required=True);p.add_argument('--new',type=Path,required=True);p.add_argument('--out',type=Path,required=True);a=p.parse_args()
out=a.out.resolve();out.mkdir(parents=True,exist_ok=False)
env={k:v for k,v in os.environ.items() if not k.startswith('LIGHTER_') and k not in ('DOCKER_HOST','DOCKER_CONTEXT','DOCKER_CONFIG')}
archives={'050':a.old.resolve(),'051':a.new.resolve()};roots={};hashes={}
for version,archive in archives.items():
 with archive.open('rb') as f:hashes[version]=hashlib.file_digest(f,'sha256').hexdigest()
 dest=out/('archive-'+version);dest.mkdir();sp.run(['tar','-xzf',archive,'-C',dest],check=True)
 roots[version]=next(dest.glob('lighter-*'))
(out/'artifacts.json').write_text(json.dumps(hashes,indent=2)+'\n')
for saved in [False,True]:
 profile=out/'saved' if saved else out
 if saved:
  profile.mkdir();(profile/'artifacts.json').write_text(json.dumps(hashes,indent=2)+'\n')
 for index,version in enumerate(['050','051','051','050'],1):
  root=roots[version];label=f'{index}-{version}'
  with (profile/(label+'.log')).open('x') as log:
   sp.run(['python3','benchmarks/guard.py','--target','lighter','--allow-program',str(root/'share/lighter/lighter.app/Contents/MacOS/lighter'),'--log',str(profile/(label+'-guard.jsonl')),'--timeout','900','--quiet','--','python3','scripts/records/record-boot.py','--cli',str(root/'bin/lighter'),'--guest',str(root/'share/lighter'),'--out',str(profile/label),'--memory','4096','--cpus','8','--reps','5','--image','alpine:3.21',*(['--saved-container'] if saved else [])],env=env,stdout=log,stderr=sp.STDOUT,check=True)
  print('PASS',('saved' if saved else 'image-only'),label,flush=True)
 if saved:(profile/'exit-code').write_text('0\n')
(out/'exit-code').write_text('0\n')
