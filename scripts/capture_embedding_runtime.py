#!/usr/bin/env python3
"""Capture a macOS CPU embedding runtime from local Python packages, read-only.

Copies the interpreter, standard library, declared package dependency closure, and
non-system Mach-O dependencies. Rewrites/signs only copies. A relocated isolated
CPU import/forward fixture must pass before atomic publication. No weights download
or semantic-quality qualification occurs. This is a private, host-specific candidate.
"""
import argparse
import hashlib
import json
import os
import platform
import shutil
import struct
import subprocess
import tempfile
from pathlib import Path

INVENTORY = r'''
import importlib.metadata,json,sys,sysconfig
from pathlib import Path
from packaging.requirements import Requirement
from packaging.markers import default_environment
pending=['torch','transformers','tokenizers','safetensors','numpy','huggingface-hub']
packages={};files={}
def add(target,source):
 source=Path(source)
 if '__pycache__' in source.parts or source.suffix=='.pyc' or not source.is_file():return
 if target in files and Path(files[target]).resolve()!=source.resolve():raise ValueError('Conflicting distribution files')
 files[target]=str(source.resolve())
while pending:
 name=pending.pop().lower().replace('_','-')
 if name in packages:continue
 distribution=importlib.metadata.distribution(name)
 packages[name]={'version':distribution.version,'license':distribution.metadata.get('License-Expression') or distribution.metadata.get('License')}
 for item in distribution.files or []:
  relative=Path(str(item))
  if relative.is_absolute() or '..' in relative.parts:continue
  add('python/lib/python%d.%d/site-packages/'%sys.version_info[:2]+relative.as_posix(),distribution.locate_file(item))
 for text in distribution.requires or []:
  dependency=Requirement(text)
  if dependency.marker is None or dependency.marker.evaluate(default_environment()|{'extra':''}):pending.append(dependency.name)
stdlib=Path(sysconfig.get_path('stdlib')).resolve()
for source in stdlib.rglob('*'):
 relative=source.relative_to(stdlib)
 if 'site-packages' not in relative.parts:add('python/lib/python%d.%d/'%sys.version_info[:2]+relative.as_posix(),source)
executable=Path(sys._base_executable).resolve()
framework_app=Path(sys.base_prefix)/'Resources/Python.app/Contents/MacOS/Python'
if framework_app.is_file():executable=framework_app.resolve()
version='%d.%d'%sys.version_info[:2]
add('python/bin/python'+version,executable)
print(json.dumps(dict(files=files,packages=packages,version=version,executable=str(executable),prefix=str(Path(sys.base_prefix).resolve()),stdlib=str(stdlib),runtime='python=%d.%d.%d;torch=%s;transformers=%s;threads=2;cpu'%(*sys.version_info[:3],packages['torch']['version'],packages['transformers']['version']))))
'''

PROBE = r'''
import json,sys
from pathlib import Path
root=Path(sys.argv[1]).resolve()
assert Path(sys.executable).resolve().is_relative_to(root),sys.executable
assert Path(sys.prefix).resolve().is_relative_to(root),(sys.prefix,root)
assert all(Path(path).resolve().is_relative_to(root) for path in sys.path),sys.path
import torch,transformers,numpy,tokenizers,safetensors
from transformers import BertConfig,BertModel
for module in [torch,transformers,numpy,tokenizers,safetensors]:
 assert Path(module.__file__).resolve().is_relative_to(root),module.__file__
torch.set_num_threads(2);torch.manual_seed(0)
model=BertModel(BertConfig(vocab_size=32,hidden_size=16,num_hidden_layers=1,num_attention_heads=2,intermediate_size=32,max_position_embeddings=64)).to('cpu').eval()
with torch.no_grad():result=model(input_ids=torch.tensor([[1,2,3]],device='cpu')).last_hidden_state
assert tuple(result.shape)==(1,3,16) and torch.isfinite(result).all()
print(json.dumps(dict(isolatedPrefix=True,localImports=True,device=str(result.device),shape=list(result.shape),finite=True,fixture='tiny-random-weight-bert-not-semantic-model',python=sys.version.split()[0],torch=torch.__version__,transformers=transformers.__version__)))
'''

BOOTSTRAP = r'''
import runpy,sys
from pathlib import Path
root=Path(__file__).resolve().parent
assert Path(sys.executable).resolve().is_relative_to(root),sys.executable
assert Path(sys.prefix).resolve().is_relative_to(root),sys.prefix
assert all(Path(path).resolve().is_relative_to(root) for path in sys.path),sys.path
site=root/'python/lib'/('python%d.%d'%sys.version_info[:2])/'site-packages'
sys.path.insert(0,str(site))
arguments=sys.argv[1:]
if not arguments or arguments[0]=='-':
 sys.argv=['-']+arguments[1:]
 exec(compile(sys.stdin.read(),'<stdin>','exec'),{'__name__':'__main__','__file__':'<stdin>'})
elif arguments[0]=='-c':
 sys.argv=['-c']+arguments[2:]
 exec(compile(arguments[1],'<string>','exec'),{'__name__':'__main__'})
elif arguments[0]=='-m':
 sys.argv=arguments[1:]
 runpy.run_module(arguments[1],run_name='__main__',alter_sys=True)
elif not arguments[0].startswith('-'):
 sys.argv=arguments
 runpy.run_path(arguments[0],run_name='__main__')
else:raise SystemExit('Runtime accepts a worker script, -c, -m, or stdin')
'''


def digest(path):
    value=hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda:stream.read(65536),b''):value.update(block)
    return value.hexdigest()


def command(args,timeout=60,env=None,cwd=None):
    result=subprocess.run(list(map(str,args)),capture_output=True,text=True,timeout=timeout,env=env,cwd=cwd)
    if result.returncode:raise RuntimeError(f'{args[0]} failed: {result.stderr.strip()}')
    return result.stdout


def macho_type(path):
    with path.open('rb') as stream:header=stream.read(16)
    if len(header)<16:return None
    magic=header[:4]
    if magic in (b'\xcf\xfa\xed\xfe',b'\xce\xfa\xed\xfe'):return struct.unpack('<I',header[12:16])[0]
    if magic in (b'\xfe\xed\xfa\xcf',b'\xfe\xed\xfa\xce'):return struct.unpack('>I',header[12:16])[0]
    if magic in (b'\xca\xfe\xba\xbe',b'\xca\xfe\xba\xbf'):
        # The captured Homebrew/PyPI executable architecture is checked by execution.
        return 0
    return None


def load_commands(path):
    lines=command(['/usr/bin/otool','-l',path]).splitlines()
    loads=[];rpaths=[];identity=None
    kind=None
    for line in lines:
        line=line.strip()
        if line.startswith('cmd '):kind=line[4:]
        elif kind=='LC_RPATH' and line.startswith('path '):rpaths.append(line[5:].split(' (offset',1)[0])
        elif line.startswith('name ') and kind in ('LC_LOAD_DYLIB','LC_LOAD_WEAK_DYLIB','LC_REEXPORT_DYLIB','LC_LOAD_UPWARD_DYLIB','LC_ID_DYLIB'):
            name=line[5:].split(' (offset',1)[0]
            if kind=='LC_ID_DYLIB':identity=name
            else:loads.append(name)
    return loads,list(dict.fromkeys(rpaths)),identity


def system_dependency(name):
    return name.startswith(('/usr/lib/','/System/Library/'))


def resolve_dependency(name,source,rpaths,executable):
    def expand(value):
        if value.startswith('@loader_path/'):return source.parent/value.removeprefix('@loader_path/')
        if value.startswith('@executable_path/'):return executable.parent/value.removeprefix('@executable_path/')
        if value=='@loader_path':return source.parent
        if value=='@executable_path':return executable.parent
        return Path(value)
    if name.startswith('@rpath/'):
        candidates=[expand(rpath)/name.removeprefix('@rpath/') for rpath in rpaths]
    else:candidates=[expand(name)]
    existing={path.resolve() for path in candidates if path.is_file()}
    if len(existing)!=1:raise ValueError(f'Cannot resolve unique native dependency {name} in {source.name}')
    return existing.pop()


def capture(inventory,stage,probe_code=PROBE):
    mapping={};original_hashes={};native=[]
    def copy(source,target):
        source=source.resolve()
        target.parent.mkdir(parents=True,exist_ok=True)
        expected=digest(source)
        shutil.copyfile(source,target)
        target.chmod(0o700 if source.stat().st_mode&0o111 else 0o600)
        if digest(target)!=expected or digest(source)!=expected:raise ValueError('Runtime input changed during capture')
        original_hashes[str(target.relative_to(stage))]=expected
        mapping.setdefault(source,target)
        if macho_type(target) is not None:native.append((source,target))
    for index,(relative,source) in enumerate(sorted(inventory['files'].items())):
        path=Path(relative)
        if path.is_absolute() or any(part in ('..','.') for part in path.parts):raise ValueError('Invalid runtime path')
        copy(Path(source),stage/path)
        if index and index%5000==0:print(f'Captured {index} runtime files',flush=True)
    executable=Path(inventory['executable']).resolve()
    dependencies=[];position=0
    while position<len(native):
        source,target=native[position];position+=1
        loads,rpaths,identity=load_commands(source)
        main=source if macho_type(source)==2 else executable
        for name in loads:
            if system_dependency(name):continue
            dependency=resolve_dependency(name,source,rpaths,main)
            if dependency not in mapping:
                destination=stage/'external'/dependency.name
                if destination.exists():
                    if digest(destination)!=digest(dependency):raise ValueError('Conflicting external library basenames')
                    mapping[dependency]=destination
                else:copy(dependency,destination)
            rewritten='@loader_path/'+os.path.relpath(mapping[dependency],target.parent)
            if name!=rewritten:command(['/usr/bin/install_name_tool','-change',name,rewritten,target])
        for rpath in rpaths:command(['/usr/bin/install_name_tool','-delete_rpath',rpath,target])
        if identity:command(['/usr/bin/install_name_tool','-id','@rpath/'+target.name,target])
        command(['/usr/bin/codesign','--force','--sign','-',target])
        # Inspect copied commands, never relying on a source-checkout RPATH.
        copied_loads,copied_rpaths,_=load_commands(target)
        if copied_rpaths:raise ValueError('Captured binary retains an RPATH')
        for name in copied_loads:
            if system_dependency(name):continue
            if not name.startswith('@loader_path/'):raise ValueError('Captured binary has an external dependency')
            resolved=(target.parent/name.removeprefix('@loader_path/')).resolve()
            if not resolved.is_relative_to(stage.resolve()) or not resolved.is_file():raise ValueError('Captured dependency escapes runtime')
        dependencies.append(dict(artifact=str(target.relative_to(stage)),dependencies=copied_loads))
        if position%20==0:print(f'Relocated {position} native runtime objects',flush=True)
    version=inventory['version']
    (stage/'python/lib'/('python'+version)/'lib-dynload').mkdir(parents=True,exist_ok=True)
    (stage/'runtime_bootstrap.py').write_text(BOOTSTRAP)
    (stage/'python-runtime').write_text('''#!/bin/sh
set -eu
runtime_dir=${0%/*}
if [ "$runtime_dir" = "$0" ]; then runtime_dir=.; fi
case "$runtime_dir" in /*) ;; *) runtime_dir="./$runtime_dir" ;; esac
runtime_dir=$(CDPATH= cd -P "$runtime_dir" && pwd)
exec "$runtime_dir/python/bin/python'''+version+'''" -I -B -S "$runtime_dir/runtime_bootstrap.py" "$@"
''')
    (stage/'python-runtime').chmod(0o700)
    # -I/-S and the bootstrap prevent user/sitecustomize/.pth paths from supplying imports.
    environment={'PATH':'/usr/bin:/bin','HF_HUB_OFFLINE':'1','TRANSFORMERS_OFFLINE':'1',
                 'OMP_NUM_THREADS':'2','MKL_NUM_THREADS':'2','TOKENIZERS_PARALLELISM':'false'}
    with tempfile.TemporaryDirectory(prefix='know-things-runtime-probe-') as working:
        probe=json.loads(command([stage/'python-runtime','-c',probe_code,stage],env=environment,cwd=working))
    source_id=hashlib.sha256(json.dumps(original_hashes,sort_keys=True,separators=(',',':')).encode()).hexdigest()
    return dict(runtime=inventory['runtime'],pythonVersion=version,packages=inventory['packages'],
                sourceFilesSha256=source_id,sourceFileCount=len(original_hashes),nativeObjects=dependencies,
                relocationProbe=probe,qualification=dict(semanticModelQualified=False,distributionLicensing='unreviewed-private-candidate',
                scope=probe.get('fixture','runtime probe on this host only')))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--python',type=Path,required=True)
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--destination',type=Path,required=True)
    args=parser.parse_args()
    if platform.system()!='Darwin':parser.error('This capture adapter currently supports macOS only')
    destination=Path(os.path.abspath(args.destination))
    if os.path.lexists(destination):parser.error('Destination must be new')
    binary=args.binary.resolve()
    environment=dict(os.environ,PYTHONDONTWRITEBYTECODE='1')
    inventory=json.loads(command([args.python.absolute(),'-B','-c',INVENTORY],env=environment))
    # Versioned Venv paths must stay intact when invoking the source interpreter.
    # args.python is intentionally used rather than its symlink target below.
    if not inventory['files']:raise ValueError('Empty runtime inventory')
    protected=[args.python.absolute().parents[1].resolve(),Path(inventory['prefix']),Path(inventory['stdlib'])]
    if any(destination.resolve()==root or root in destination.resolve().parents for root in protected):
        parser.error('Output must be outside source runtime directories')
    for source in inventory['files'].values():
        if Path(source).resolve()==destination or destination.resolve() in Path(source).resolve().parents:
            parser.error('Output must not contain source runtime files')
    os.umask(0o077)
    with tempfile.TemporaryDirectory(prefix='know-things-runtime-capture-') as working:
        stage=Path(working)/'runtime';stage.mkdir(mode=0o700)
        manifest=capture(inventory,stage)
        artifacts={str(path.relative_to(stage)):digest(path) for path in sorted(stage.rglob('*')) if path.is_file()}
        encoded=json.dumps(dict(schemaVersion=1,kind='embedding_runtime',entrypoint='python-runtime',arguments=[],artifacts=artifacts,manifest=manifest),indent=2)+'\n'
        if len(encoded.encode())>8*1024*1024:raise ValueError('Runtime manifest exceeds 8 MiB')
        (stage/'bundle.json').write_text(encoded)
        destination.parent.mkdir(parents=True,exist_ok=True)
        command([binary,'assets','import',stage,destination],timeout=120)
    print(json.dumps(dict(runtimeBundle=str(destination),bundleManifestSha256=digest(destination/'bundle.json'),
                         runtime=manifest['runtime'],files=len(artifacts),relocationProbe=manifest['relocationProbe'],semanticModelQualified=False)))


if __name__=='__main__':main()
