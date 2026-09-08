"""Verify final release bytes against the exact qualified executable/payload."""
import argparse,hashlib,json,pathlib,shutil,subprocess as sp,tempfile
p=argparse.ArgumentParser();p.add_argument('--archive',type=pathlib.Path,required=True);p.add_argument('--bootstrap',type=pathlib.Path,required=True);p.add_argument('--qualified-cli',type=pathlib.Path,required=True);p.add_argument('--record-environment',type=pathlib.Path,required=True);p.add_argument('--output',type=pathlib.Path,required=True);a=p.parse_args()
def digest(path):
 with path.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
env=json.loads(a.record_environment.read_text());original_cli=env['cli_sha256'];assert digest(a.qualified_cli)==original_cli
result=dict(archive_sha256=digest(a.archive),bootstrap_sha256=digest(a.bootstrap),qualification_source=env['source'],runtime_source=env['source'],guest={},unsigned_code={})
with tempfile.TemporaryDirectory(prefix='lighter-051-equivalence-') as temp:
 work=pathlib.Path(temp);sp.run(['tar','-xzf',str(a.archive.resolve()),'-C',temp],check=True);root=work/'lighter-0.5.1';app=root/'share/lighter/lighter.app'
 req='=anchor apple generic and identifier "dev.lighter.machine" and certificate leaf[subject.OU] = "N7N6BNF95K" and certificate leaf[field.1.2.840.113635.100.6.1.13] exists'
 sp.run(['codesign','--verify','--strict','--deep','-R',req,str(app)],check=True)
 sp.run(['codesign','--verify','--strict',str(root/'bin/lighter')],check=True)
 sp.run(['spctl','--assess','--type','execute',str(app)],check=True)
 sp.run(['xcrun','stapler','validate',str(app)],check=True)
 manifest=json.loads((app/'Contents/Resources/release.json').read_text());assert manifest['version']=='0.5.1';assert manifest['kernel_version']=='6.18.49';assert manifest['data_epoch']==1
 for name,sha in manifest['files'].items():assert digest(root/name)==sha,name
 assert digest(a.bootstrap)==digest(root/'bin/lighter')
 for name in ['Image','rootfs.ext4']:
  sha=digest(root/'share/lighter'/name);assert sha==env['payload'][name];result['guest'][name]=sha
 qualified=work/'qualified';shutil.copyfile(a.qualified_cli,qualified);sp.run(['codesign','--remove-signature',str(qualified)],check=True)
 for name in ['bin/lighter','share/lighter/lighter.app/Contents/MacOS/lighter']:
  sp.run(['codesign','--remove-signature',str(root/name)],check=True);sha=digest(root/name);assert sha==digest(qualified),name;result['unsigned_code'][name]=sha
result['release_source']=sp.check_output(['git','rev-parse','HEAD'],text=True).strip();a.output.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2));print('PASS: signed final code and guest payload equal the qualified runtime')
