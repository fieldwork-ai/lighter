from pathlib import Path
import os, subprocess as sp, json, time, tempfile, socket, http.client, shutil
root=Path.cwd();out=root/'.logs/051/start-memory-67d787b';out.mkdir(exist_ok=False)
cli=str(root/'.logs/051/lighter-67d787b');guest=str(root/'.logs/051/guest-candidate')
base={k:v for k,v in os.environ.items() if not k.startswith('LIGHTER_') and k not in ('DOCKER_HOST','DOCKER_CONTEXT','DOCKER_CONFIG')}
base.update(LIGHTER_GUEST_DIR=guest,LIGHTER_BOOT_TIMING='1')
records=[]
for memory in [8192,12288,16384,32768]:
 home=Path(tempfile.mkdtemp(prefix='lighter-051-memory-',dir='/private/tmp'))
 env=dict(base,LIGHTER_HOME=str(home),LIGHTER_BACKGROUND_RAM='1')
 dk=['docker','--host','unix://'+str(home/'docker.sock')]
 (out/f'{memory}-home').write_text(str(home)+'\n')
 with (out/f'{memory}.log').open('w') as log:
  def run(args):
   p=sp.run(args,env=env,capture_output=True,text=True,timeout=120)
   log.write(p.stdout+p.stderr);log.flush();p.check_returncode();return p.stdout.strip()
  def record(mode,value):
   row=dict(memory_mib=memory,mode=mode,memtotal_kib=int(value));records.append(row);print(row,flush=True)
   (out/'results.json').write_text(json.dumps(records,indent=2)+'\n')
  try:
   run([cli,'config','--cpus','8','--memory',str(memory),'--disk','128'])
   run([cli,'start']);run(dk+['pull','alpine:3.21']);run([cli,'stop'])
   for mode in ['eager','background-fragmented','background-restored']:
    env['LIGHTER_BACKGROUND_RAM']='0' if mode=='eager' else '1'
    run([cli,'start'])
    command=['sh','-c',"awk '/MemTotal/ {print $2}' /proc/meminfo > /boot-memory-kib; sleep 10000"]
    if mode=='eager':
     run(dk+['run','--rm','alpine:3.21','sh','-c',"awk '/MemTotal/ {print $2}' /proc/meminfo"])
     expected=run(dk+['run','--rm','alpine:3.21','sh','-c',"awk '/MemTotal/ {print $2}' /proc/meminfo"])
     record(mode,expected)
    elif mode=='background-fragmented':
     # One-byte writes exercise request markers split across socket reads.
     body=json.dumps(dict(Image='alpine:3.21',Cmd=command,HostConfig=dict(RestartPolicy=dict(Name='always')))).encode()
     request=b'POST /v1.51/containers/create?name=boot-memory-proof HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: '+str(len(body)).encode()+b'\r\nConnection: close\r\n\r\n'+body
     with socket.socket(socket.AF_UNIX) as sock:
      sock.settimeout(45);sock.connect(str(home/'docker.sock'))
      for byte in request:
       sock.sendall(bytes([byte]));time.sleep(.001)
      response=http.client.HTTPResponse(sock);response.begin();content=response.read();assert response.status==201,(response.status,content)
     run(dk+['start','boot-memory-proof'])
     value=run(dk+['exec','boot-memory-proof','cat','/boot-memory-kib']);record(mode,value)
     assert abs(int(value)-int(expected))<256,(memory,mode,value,expected)
    else:
     value=run(dk+['exec','boot-memory-proof','cat','/boot-memory-kib']);record(mode,value)
     assert abs(int(value)-int(expected))<256,(memory,mode,value,expected)
     run(dk+['rm','-f','boot-memory-proof'])
    shutil.copy2(home/'machine.log',out/f'{memory}-{mode}-machine.log')
    run([cli,'stop']);time.sleep(1)
  finally:
   sp.run([cli,'stop'],env=env,stdout=log,stderr=sp.STDOUT,timeout=120)
(out/'exit-code').write_text('0\n')
