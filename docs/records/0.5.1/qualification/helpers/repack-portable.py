"""Repack retained notarized payloads without changing any signed file."""
from pathlib import Path
import hashlib,json,os,subprocess as sp,tarfile,tempfile
base=Path('.logs/051').resolve();root=Path.cwd();records=[]
for source,dest in [(base/'rejected-pax-archives/lighter-0.5.1-arm64.tar.gz',root/'dist/lighter-0.5.1-arm64.tar.gz'),(base/'rejected-pax-archives/future-good.tar.gz',base/'fixtures/future-good.tar.gz'),(base/'rejected-pax-archives/future-bad.tar.gz',base/'fixtures/future-bad.tar.gz')]:
 with tempfile.TemporaryDirectory(prefix='lighter-portable-archive-') as tmp:
  work=Path(tmp);sp.run(['tar','-xzf',source,'-C',work],check=True);release=next(work.glob('lighter-*'))
  def contents():
   data={}
   for p in sorted(release.rglob('*')):
    if p.is_file():
     with p.open('rb') as f:data[str(p.relative_to(release))]=dict(sha256=hashlib.file_digest(f,'sha256').hexdigest(),mode=p.stat().st_mode&0o777)
   return data
  before=contents();archive=work/'portable.tar.gz'
  sp.run(['tar','--format','ustar','-czf',archive,'-C',work,release.name],env=dict(os.environ,COPYFILE_DISABLE='1'),check=True)
  with tarfile.open(archive) as tar:
   info=tar.getmember(release.name+'/share/lighter/rootfs.ext4');assert info.isfile() and not info.sparse and not info.pax_headers
  with (base/('repack-'+release.name+'.log')).open('w') as log:
   sp.run([root/'dist/lighter-0.5.1-arm64','install-archive','--archive',archive,'--prefix',work/'install'],env=dict(os.environ,LIGHTER_HOME=str(work/'home')),stdout=log,stderr=sp.STDOUT,check=True)
  installed=work/'install/releases'/release.name.removeprefix('lighter-')
  for name,entry in before.items():
   with (installed/name).open('rb') as f:assert hashlib.file_digest(f,'sha256').hexdigest()==entry['sha256'],name
  assert contents()==before
  with archive.open('rb') as f:sha=hashlib.file_digest(f,'sha256').hexdigest()
  # Preserve originals separately; no asset has been published.
  import shutil
  shutil.copyfile(archive,dest)
  records.append(dict(source=str(source),destination=str(dest),sha256=sha,files_unchanged=before,installer_passed=True))
  print('PASS',dest.name,sha,flush=True)
 (base/'portable-repack.json').write_text(json.dumps(records,indent=2)+'\n')
