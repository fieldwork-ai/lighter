import subprocess,json,pathlib
r=pathlib.Path(__file__).parent
with (r/'exec-probe.jsonl').open('w') as f:
 for i in range(100):
  p=subprocess.run(['docker','exec','lighter-qualification-control-plane','printf','kind-exec-ok'],capture_output=True,timeout=15)
  row=dict(iteration=i,rc=p.returncode,stdout=p.stdout.decode(errors='replace'),stderr=p.stderr.decode(errors='replace'))
  f.write(json.dumps(row)+'\n');f.flush()
  if p.returncode or p.stdout!=b'kind-exec-ok':print(row,flush=True)
print('100 exec probes complete')
