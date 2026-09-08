from pathlib import Path
import os,subprocess as sp,json
root=Path.cwd();out=root/'.logs/051/m5-streams-67d787b';out.mkdir(exist_ok=False)
home=(root/'.logs/051/m5-boot-67d787b-a1/8192-home').read_text().strip()
cli=str(root/'.logs/051/lighter-67d787b')
env={k:v for k,v in os.environ.items() if not k.startswith('LIGHTER_') and k not in ('DOCKER_HOST','DOCKER_CONTEXT','DOCKER_CONFIG')}
env.update(LIGHTER_HOME=home,LIGHTER_GUEST_DIR=str(root/'.logs/051/guest-candidate'),DOCKER_HOST='unix://'+home+'/docker.sock')
with (out/'commands.log').open('w') as log:
 def run(args,timeout=120):return sp.run(args,env=env,stdout=log,stderr=sp.STDOUT,check=True,timeout=timeout)
 try:
  run([cli,'start']);run(['docker','pull','python:3.12-alpine'])
  inspect=sp.check_output(['docker','image','inspect','python:3.12-alpine'],env=env,text=True)
  (out/'image.json').write_text(inspect)
  image=json.loads(inspect)[0]['Id']
  run(['python3','scripts/test-stream-output.py','--image',image,'--reps','100','--workers','8','--output',str(out/'results.jsonl')],timeout=600)
  (out/'exit-code').write_text('0\n')
 finally:run([cli,'stop'])
