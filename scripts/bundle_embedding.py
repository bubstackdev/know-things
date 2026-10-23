#!/usr/bin/env python3
"""Build a pinned offline embedding bundle from local model and runtime assets.

Select an external research interpreter or a captured runtime bundle. Model Python
code is never copied or executed. Assembly does not establish semantic quality.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from package_installation import ROOT, command, copy_checked, digest, capture_bundle
from bundle_layout import EMBEDDING_RUNTIME_LAUNCHER


def runtime_identity(interpreter):
    environment=dict(os.environ,PYTHONDONTWRITEBYTECODE='1',HF_HUB_OFFLINE='1',TRANSFORMERS_OFFLINE='1')
    result=subprocess.run([str(interpreter),'-c','import sys,torch,transformers;print(f"python={sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro};torch={torch.__version__};transformers={transformers.__version__};threads=2;cpu")'],env=environment,capture_output=True,text=True,timeout=60)
    if result.returncode:raise RuntimeError(result.stderr)
    return result.stdout.strip()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--model',type=Path,required=True)
    parser.add_argument('--destination',type=Path,required=True)
    runtime=parser.add_mutually_exclusive_group(required=True)
    runtime.add_argument('--python',type=Path)
    runtime.add_argument('--runtime-bundle',type=Path)
    parser.add_argument('--binary',type=Path,default=ROOT/'target/release/know-things')
    parser.add_argument('--name',required=True)
    parser.add_argument('--license',required=True)
    parser.add_argument('--dimensions',type=int,required=True)
    parser.add_argument('--max-input-tokens',type=int,required=True)
    parser.add_argument('--query-prefix',default='')
    parser.add_argument('--document-prefix',default='')
    args=parser.parse_args()
    destination=Path(os.path.abspath(args.destination))
    if os.path.lexists(destination):parser.error('Destination must be new')
    if not 0<args.dimensions<=65536 or not 4<=args.max_input_tokens<=32768:
        parser.error('Dimensions must be 1–65536 and input tokens 4–32768')
    for source in [args.model,args.runtime_bundle]:
        if source and (destination.resolve()==source.resolve() or source.resolve() in destination.resolve().parents):
            parser.error('Destination must be outside model/runtime inputs')
    binary=args.binary.resolve()
    if not binary.is_file():parser.error('Build the core or supply --binary for verified publication')
    interpreter=args.python.absolute() if args.python else None
    if interpreter and any(character.isspace() for character in str(interpreter)):
        parser.error('External-interpreter shebangs cannot contain whitespace; use a runtime bundle')
    config=json.loads((args.model/'config.json').read_text())
    os.umask(0o077)
    with tempfile.TemporaryDirectory(prefix='know-things-embedding-bundle-') as working:
        stage=Path(working)/'embedding';stage.mkdir(mode=0o700)
        if args.runtime_bundle:
            print('Capturing verified runtime',file=sys.stderr,flush=True)
            captured=capture_bundle(binary,args.runtime_bundle.resolve(),stage/'runtime','embedding_runtime')
            if captured['entrypoint']!='python-runtime' or captured['arguments']:
                raise ValueError('Unsupported captured runtime entrypoint')
            runtime=runtime_identity(stage/'runtime/python-runtime')
            if runtime!=captured['manifest']['runtime']:raise ValueError('Captured runtime identity mismatch')
            entrypoint='embedding'
            (stage/entrypoint).write_text(EMBEDDING_RUNTIME_LAUNCHER);(stage/entrypoint).chmod(0o700)
        else:
            runtime=runtime_identity(interpreter)
            entrypoint='embedding.py'
        model=stage/'model';model.mkdir()
        allowed={'.json','.safetensors','.txt','.model','.jinja'}
        for source in sorted(args.model.rglob('*')):
            if source.is_symlink():raise ValueError('Model inputs cannot contain symlinks')
            if source.is_file() and (source.suffix in allowed or source.name in {'LICENSE','NOTICE'}):
                copy_checked(source,model/source.relative_to(args.model))
        if not list(model.rglob('*.safetensors')):raise ValueError('Model must include safetensors weights')
        if json.loads((model/'config.json').read_text())!=config:raise ValueError('Model configuration changed during capture')
        worker_source=ROOT/'workers/embedding.py'
        worker=stage/'embedding.py'
        copy_checked(worker_source,worker)
        if interpreter:
            worker.write_text('#!'+str(interpreter)+'\n'+worker.read_text().split('\n',1)[1])
            worker.chmod(0o700)
        artifacts={str(path.relative_to(stage)):digest(path) for path in sorted(stage.rglob('*')) if path.is_file()}
        manifest=dict(name=args.name,artifacts=artifacts,tokenizer='model/tokenizer.json',architecture=config['model_type'],
            quantization='as-provisioned',runtime=runtime,queryPrefix=args.query_prefix,documentPrefix=args.document_prefix,
            pooling='masked-mean-token-weighted-windows-v2-native-postprocessor',normalization='l2',dimensions=args.dimensions,
            maxInputTokens=args.max_input_tokens,license=args.license)
        (stage/'bundle.json').write_text(json.dumps(dict(schemaVersion=1,kind='embedding',entrypoint=entrypoint,
            arguments=[],artifacts=artifacts,manifest=manifest),indent=2)+'\n')
        destination.parent.mkdir(parents=True,exist_ok=True)
        print('Publishing verified embedding bundle',file=sys.stderr,flush=True)
        command([binary,'assets','import',stage,destination],timeout=600)
    print(destination)


if __name__=='__main__':main()
