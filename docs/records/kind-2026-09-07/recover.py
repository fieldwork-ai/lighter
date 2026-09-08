import subprocess as sp,time,pathlib,json
r=pathlib.Path(__file__).parent
attempts=[];passed=False
for attempt in range(60):
 try:
  p=sp.run(['kubectl','--request-timeout=5s','-n','qualification','exec','client','--','wget','-T','3','-qO-','http://web'],text=True,capture_output=True,timeout=8)
  c=sp.run(['curl','--noproxy','*','-fsS','--max-time','3','http://127.0.0.1:18880/'],text=True,capture_output=True,timeout=5)
  row=dict(attempt=attempt,pod_returncode=p.returncode,pod_output=p.stdout,pod_error=p.stderr,host_returncode=c.returncode,host_output=c.stdout,host_error=c.stderr)
  attempts.append(row)
  if p.returncode==c.returncode==0 and p.stdout.strip()==c.stdout.strip()=='lighter-kind-qualified':passed=True;break
 except Exception as e:attempts.append(dict(attempt=attempt,error=str(e)))
 time.sleep(1)
(r/'multi-recovery.json').write_text(json.dumps(dict(passed=passed,attempts=attempts),indent=2))
assert passed,'multi-node service did not recover'
p=sp.run(['kubectl','--request-timeout=10s','-n','qualification','exec','deployment/web','--','cat','/data/token'],text=True,capture_output=True,timeout=15)
assert p.returncode==0 and p.stdout.strip()=='multi-persistent-token',(p.returncode,p.stdout,p.stderr)
assert (r/'share/from-pod').read_text().strip()=='guest-wrote-token'
print('PASS: cross-node service, host NodePort, PVC data and Mac share after exact-release VM restart')
