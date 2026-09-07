import subprocess as sp,pathlib,time,json,urllib.request
r=pathlib.Path(__file__).parent
results=[]
def command(args):return sp.check_output(args,stderr=sp.STDOUT,text=True,timeout=45)
def k(*args):return command(['kubectl','--request-timeout=10s',*args])
def check(name,fn):
 try:
  result=fn(); results.append(dict(name=name,passed=True,output=result));print('PASS',name,flush=True)
 except Exception as e:
  results.append(dict(name=name,passed=False,error=str(e),output=getattr(e,'output','')));print('FAIL',name,str(e),flush=True)
 (r/'functional-results.json').write_text(json.dumps(results,indent=2))
def equals(actual,expected):
 assert actual.strip()==expected,(actual,expected)
 return actual
def ready():
 deadline=time.monotonic()+120
 while time.monotonic()<deadline:
  try:
   j=json.loads(k('-n','qualification','get','deployment','web','-o','json'))
   if j['status'].get('availableReplicas')==1:return 'web available'
  except Exception:pass
  time.sleep(2)
 raise RuntimeError('web not available')
check('deployment after exact-release VM restart',ready)
check('PVC data after VM restart',lambda:equals(k('-n','qualification','exec','deployment/web','--','cat','/data/token'),'persistent-token'))
check('cluster service DNS and HTTP (nftables)',lambda:equals(k('-n','qualification','exec','deployment/web','--','wget','-T','5','-qO-','http://web'),'lighter-kind-qualified'))
check('external DNS',lambda:k('-n','qualification','exec','deployment/web','--','nslookup','example.com'))
check('pod outbound HTTP',lambda:k('-n','qualification','exec','deployment/web','--','wget','-T','5','-qO-','http://example.com'))
check('host NodePort mapping (nftables)',lambda:equals(urllib.request.urlopen('http://127.0.0.1:18880/',timeout=5).read().decode(),'lighter-kind-qualified'))
check('pod logs',lambda:equals(k('-n','qualification','logs','deployment/web'),'server-started'))
with (r/'port-forward.log').open('w') as out:
 p=sp.Popen(['kubectl','--request-timeout=10s','-n','qualification','port-forward','service/web','18881:80','--address','127.0.0.1'],stdout=out,stderr=out)
 try:
  def forward():
   for i in range(30):
    if 'Forwarding from' in (r/'port-forward.log').read_text():break
    time.sleep(.2)
   return equals(urllib.request.urlopen('http://127.0.0.1:18881/',timeout=5).read().decode(),'lighter-kind-qualified')
  check('kubectl port-forward',forward)
 finally:
  p.terminate()
  try:p.wait(5)
  except sp.TimeoutExpired:p.kill();p.wait()
def replace_pod():
 pods=json.loads(k('-n','qualification','get','pods','-l','app=web','-o','json'))
 old=pods['items'][0]['metadata']['uid'];name=pods['items'][0]['metadata']['name']
 k('-n','qualification','delete','pod',name,'--wait=false')
 deadline=time.monotonic()+120
 while time.monotonic()<deadline:
  pods=json.loads(k('-n','qualification','get','pods','-l','app=web','-o','json'))
  for pod in pods['items']:
   if pod['metadata']['uid']!=old and any(c['type']=='Ready' and c['status']=='True' for c in pod.get('status',{}).get('conditions',[])):
    return equals(k('-n','qualification','exec',pod['metadata']['name'],'--','cat','/data/token'),'persistent-token')
  time.sleep(2)
 raise RuntimeError('replacement pod not ready')
check('PVC data survives pod replacement',replace_pod)
print(json.dumps(results,indent=2))
