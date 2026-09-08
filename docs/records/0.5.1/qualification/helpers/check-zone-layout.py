from pathlib import Path
import os,subprocess as sp,json,tempfile
root=Path.cwd();out=root/'.logs/051/zone-layout-67d787b';out.mkdir(exist_ok=False)
cli=str(root/'.logs/051/lighter-67d787b');home=Path(tempfile.mkdtemp(prefix='lighter-051-zones-',dir='/private/tmp'))
env={k:v for k,v in os.environ.items() if not k.startswith('LIGHTER_') and k not in ('DOCKER_HOST','DOCKER_CONTEXT','DOCKER_CONFIG')}
env.update(LIGHTER_HOME=str(home),LIGHTER_GUEST_DIR=str(root/'.logs/051/guest-candidate'),DOCKER_HOST=f'unix://{home}/docker.sock')
dk=['docker','--host',env['DOCKER_HOST']];(out/'home').write_text(str(home)+'\n')
with (out/'commands.log').open('w') as log:
 def run(args):
  p=sp.run(args,env=env,capture_output=True,text=True,timeout=120);log.write(p.stdout+p.stderr);log.flush();p.check_returncode();return p.stdout
 try:
  run([cli,'config','--cpus','8','--memory','4096','--disk','128'])
  for index,mode in enumerate(['eager','background','eager','background']):
   env['LIGHTER_BACKGROUND_RAM']='0' if mode=='eager' else '1'
   run([cli,'start']);run(dk+['run','-d','--name','zone-proof','alpine:3.21','sleep','120'])
   zones=run(dk+['exec','zone-proof','cat','/proc/zoneinfo'])
   (out/f'{index+1}-{mode}-zoneinfo').write_text(zones)
   lines=run(dk+['exec','zone-proof','sh','-c','for block in /sys/devices/system/memory/memory[0-9]*; do printf "%s " "${block##*/}"; cat "$block/state" "$block/valid_zones" | tr "\n" " "; echo; done'])
   (out/f'{index+1}-{mode}-blocks').write_text(lines)
   run(dk+['rm','-f','zone-proof']);run([cli,'stop'])
 finally:sp.run([cli,'stop'],env=env,stdout=log,stderr=sp.STDOUT,timeout=120)
(out/'exit-code').write_text('0\n')
