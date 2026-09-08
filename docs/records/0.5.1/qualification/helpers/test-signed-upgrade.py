"""Upgrade the real signed 0.5.0 archive in private state without changing HOME."""
import argparse,hashlib,json,os
from pathlib import Path
import subprocess as sp
import tempfile

ap=argparse.ArgumentParser();ap.add_argument('--old',type=Path,required=True);ap.add_argument('--new',type=Path,required=True);ap.add_argument('--bootstrap',type=Path,required=True);ap.add_argument('--out',type=Path,required=True);a=ap.parse_args()
root=Path.cwd();out=a.out.resolve();out.mkdir(parents=True,exist_ok=False)
a.old=a.old.resolve();a.new=a.new.resolve();a.bootstrap=a.bootstrap.resolve()
with a.old.open('rb') as source:assert hashlib.file_digest(source,'sha256').hexdigest()=='4da3fca84dbaf6d1ede81d682efbb52df64db438cfeef9679d3e60eb05e8df52'
env={k:v for k,v in os.environ.items() if not k.startswith('LIGHTER_') and k not in ['DOCKER_HOST','DOCKER_CONTEXT','DOCKER_CONFIG','GITHUB_TOKEN']}
records=[]
with tempfile.TemporaryDirectory(prefix='lighter-051-migration-',dir='/private/tmp') as directory, (out/'commands.log').open('w') as log:
 work=Path(directory);prefix=work/'installation';prefix.mkdir();path_target=work/'path';path_target.mkdir()
 env.update(LIGHTER_HOME=str(prefix),DOCKER_CONFIG=str(work/'docker-config'),DOCKER_HOST=f'unix://{prefix}/docker.sock')
 cli=prefix/'bin/lighter';dk=['docker','--host',env['DOCKER_HOST']]
 def run(args,extra=None,required=True):
  p=sp.run([str(x) for x in args],env=dict(env,**(extra or {})),capture_output=True,text=True,timeout=180)
  log.write(p.stdout+p.stderr);log.flush()
  if required:p.check_returncode()
  return p
 try:
  run(['tar','-xzf',a.old,'-C',work])
  old=work/'lighter-0.5.0/bin/lighter'
  run([old,'install-archive','--archive',a.old,'--prefix',prefix])
  config=dict(cpus=4,memory_mib=4096,disk_gib=64,shares=[],publish='localhost')
  (prefix/'config.json').write_text(json.dumps(config)+'\n');before=(prefix/'config.json').read_bytes()
  run([cli,'start','--timeout','120'])
  run(dk+['run','--rm','-v','migration-data:/data','alpine:3.21','sh','-c','echo preserved > /data/value'])
  run(dk+['run','-d','--name','migration-container','--restart','always','alpine:3.21','sh','-c',"awk '/MemTotal/ {print $2}' /proc/meminfo > /boot-memory-kib; sleep 3600"])
  old_memory=int(run(dk+['exec','migration-container','cat','/boot-memory-kib']).stdout.strip())
  old_identity=json.loads((prefix/'machine.identity').read_text())
  (out/'old-identity.json').write_text(json.dumps(old_identity,indent=2)+'\n')
  import ctypes
  lib=ctypes.CDLL('/usr/lib/libproc.dylib',use_errno=True);lib.proc_pidpath_audittoken.argtypes=[ctypes.POINTER(ctypes.c_uint32),ctypes.c_void_p,ctypes.c_uint32];lib.proc_pidpath_audittoken.restype=ctypes.c_int
  buf=ctypes.create_string_buffer(4096);token=(ctypes.c_uint32*8)(*old_identity['token']);rc=lib.proc_pidpath_audittoken(token,buf,len(buf));path=Path(os.fsdecode(buf.value))
  (out/'running-path.json').write_text(json.dumps(dict(rc=rc,errno=ctypes.get_errno(),path=str(path),exists=path.exists(),resolved=str(path.resolve()),prefix=str(prefix)),indent=2)+'\n')
  refused=run([a.bootstrap,'install-archive','--archive',a.new,'--prefix',prefix],required=False)
  assert refused.returncode!=0 and '--restart' in refused.stdout+refused.stderr
  assert json.loads((prefix/'machine.identity').read_text())==old_identity
  installer=work/'install.sh';installer.write_text((root/'scripts/install.sh').read_text().replace('/usr/local/bin',str(path_target)))
  run(['bash',installer,'--restart'],extra=dict(LIGHTER_VERSION='0.5.1',LIGHTER_INSTALL_DIR=str(prefix),LIGHTER_TARBALL_URL=a.new.as_uri(),LIGHTER_BOOTSTRAP_URL=a.bootstrap.as_uri(),GITHUB_TOKEN=''))
  assert run([cli,'--version']).stdout.strip()=='lighter 0.5.1'
  run([cli,'doctor']);run([cli,'status'])
  assert run(dk+['run','--rm','--platform','linux/amd64','alpine:3.21','uname','-m']).stdout.strip()=='x86_64'
  assert run(dk+['run','--rm','-v','migration-data:/data','alpine:3.21','cat','/data/value']).stdout.strip()=='preserved'
  assert run(dk+['inspect','-f','{{.State.Running}}','migration-container']).stdout.strip()=='true'
  new_memory=int(run(dk+['exec','migration-container','cat','/boot-memory-kib']).stdout.strip())
  assert abs(new_memory-old_memory)<256,(old_memory,new_memory)
  assert (prefix/'config.json').read_bytes()==before
  assert os.readlink(prefix/'current')=='releases/0.5.1'
  assert (prefix/'releases/0.5.0').is_dir() and not (prefix/'upgrade.json').exists()
  identity=json.loads((prefix/'machine.identity').read_text());assert identity['release_version']=='0.5.1'
  assert identity['kernel_version']=='6.18.49'
  ownership=json.loads((prefix/'share/lighter/installation.json').read_text());assert ownership['method']=='script'
  (out/'results.json').write_text(json.dumps(dict(old_version='0.5.0',new_version='0.5.1',old_memory_kib=old_memory,new_memory_kib=new_memory,configuration_preserved=True,volume_preserved=True,restart_policy_preserved=True,explicit_restart_enforced=True,amd64=True),indent=2)+'\n')
  (out/'exit-code').write_text('0\n')
  print('PASS: signed 0.5.0 -> 0.5.1 preserves configuration, data, running container and full startup RAM')
 finally:
  if cli.exists():run([cli,'status'],required=False);run([cli,'stop'],required=False)
  if (prefix/'machine.log').exists():(out/'machine.log').write_bytes((prefix/'machine.log').read_bytes())
  run(['python3',root/'scripts/records/unregister-test-bundles.py',work],required=False)
