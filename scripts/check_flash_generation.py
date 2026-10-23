#!/usr/bin/env python3
"""Capture a private native worker trace for a synthetic prompt; no corpus access."""
import argparse
import json
import os
import struct
import subprocess
import time
from pathlib import Path
from types import SimpleNamespace
from check_flash_lifecycle import read_exact
from pressure_monitor import memory_pressure_level,protect_owned_work
from evaluate_answers import write_new
from package_installation import digest,command


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ['binary','flash-bundle','generation-profile','output']:parser.add_argument('--'+name,type=Path,required=True)
    args=parser.parse_args()
    paths=[args.output,args.output.with_suffix('.stderr'),args.output.with_suffix('.pressure.json')]
    if any(p.exists() for p in paths):parser.error('All outputs must be new')
    if memory_pressure_level()!=1:raise RuntimeError('Defer GPU trial under pressure')
    flash=args.flash_bundle.resolve();root=args.generation_profile.resolve()
    for bundle in [flash,root]:command([args.binary.resolve(),'assets','verify',bundle],timeout=300)
    worker=json.loads((flash/'bundle.json').read_text());bundle=json.loads((root/'bundle.json').read_text());profile=bundle['manifest']
    control_read,control_write=os.pipe();events=[];started=time.monotonic()
    args.output.parent.mkdir(parents=True,exist_ok=True)
    with os.fdopen(os.open(paths[1],os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600),'w') as stderr:
        process=subprocess.Popen([str(flash/worker['entrypoint'])],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=stderr,pass_fds=(control_read,),env=dict(os.environ,KNOW_THINGS_ASSET_ROOT=str(flash),KNOW_THINGS_CONTROL_FD=str(control_read)))
        os.close(control_read)
        def receive(timeout=300):
            deadline=time.monotonic()+timeout;size=struct.unpack('>I',read_exact(process.stdout,4,deadline))[0]
            if not 0<size<=1024*1024:raise ValueError('Frame limit')
            event=json.loads(read_exact(process.stdout,size,deadline));events.append(event);return event
        def send(**request):
            data=json.dumps(dict(protocolVersion=1,**request)).encode();process.stdin.write(struct.pack('>I',len(data))+data);process.stdin.flush()
        failure=None
        try:
            with protect_owned_work(SimpleNamespace(process=process),paths[2]):
                hello=receive();assert hello['flashArtifactId']==worker['manifest']['flashArtifactId']
                send(requestId=1,operation='load_profile',profileId='synthetic-protocol-diagnostic',architecture=profile['architecture'],modelPath=str(root/profile['modelPath']),tokenizerPath=str(root/profile['tokenizerPath']),contextLimit=profile['contextLimit'],weightBits=profile['weightBits'])
                loaded=receive();assert loaded['event']=='profile_loaded',loaded
                prompt='<|im_start|>system\nAnswer briefly.<|im_end|>\n<|im_start|>user\nWhich cache is required? Cache policy requires Redis.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n'
                send(requestId=2,operation='tokenize',text=prompt);tokens=receive();assert tokens['event']=='tokenized',tokens
                send(requestId=3,operation='generate',prompt=prompt,expectedTokenCount=tokens['tokenCount'],completionLimit=160,deadlineMs=30000)
                while True:
                    event=receive(timeout=35)
                    if event['event'] in ['completed','failed']:break
                print(json.dumps(dict(event=event['event'],finishReason=event.get('finishReason'),generatedTokens=event.get('generatedTokens'))),flush=True)
        except Exception as error:
            failure=type(error).__name__
            raise
        finally:
            os.close(control_write)
            process.stdin.close()
            try:process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.terminate()
                try:process.wait(timeout=2)
                except subprocess.TimeoutExpired:process.kill();process.wait(timeout=2)
            process.stdout.close()
            write_new(args.output,dict(kind='private-native-generation-protocol-diagnostic',scriptSha256=digest(Path(__file__)),flashManifestSha256=digest(flash/'bundle.json'),generationManifestSha256=digest(root/'bundle.json'),events=events,failureType=failure,elapsedMs=(time.monotonic()-started)*1000,exitCode=process.returncode,actualCorpusAccessed=False,semanticQualityQualified=False,releaseQualified=False))


if __name__=='__main__':main()
