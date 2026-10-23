#!/usr/bin/env python3
"""Check a verified embedding worker with fixed synthetic text, never corpus data."""
import argparse
import hashlib
import json
import math
import os
import select
import struct
import subprocess
import tempfile
import time
from pathlib import Path
from package_installation import ROOT,command,digest


def read_exact(stream,count):
    data=bytearray();deadline=time.monotonic()+30
    while len(data)<count:
        remaining=deadline-time.monotonic()
        if remaining<=0 or not select.select([stream],[],[],remaining)[0]:raise TimeoutError('Worker fixture frame deadline')
        block=os.read(stream.fileno(),count-len(data))
        if not block:raise ValueError('Worker closed fixture frame')
        data.extend(block)
    return data


def receive(process):
    size=struct.unpack('>I',read_exact(process.stdout,4))[0]
    if not 0<size<=1024*1024:raise ValueError('Worker fixture frame limit')
    return json.loads(read_exact(process.stdout,size))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,default=ROOT/'target/release/know-things')
    parser.add_argument('--embedding-bundle',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--expected-input-tokens',type=int)
    parser.add_argument('--expected-windows',type=int)
    parser.add_argument('--repetitions',type=int,default=20)
    args=parser.parse_args()
    if not 1<=args.repetitions<=1000:parser.error('Use 1–1000 synthetic phrase repetitions')
    if args.output.exists():parser.error('Output must be new')
    root=args.embedding_bundle.resolve()
    command([args.binary.resolve(),'assets','verify',root])
    bundle=json.loads((root/'bundle.json').read_text());manifest=bundle['manifest']
    if bundle['kind']!='embedding':parser.error('Expected an embedding bundle')
    space=hashlib.sha256(json.dumps(manifest,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode()).hexdigest()
    with tempfile.TemporaryDirectory(prefix='know-things-embedding-protocol-') as working,tempfile.TemporaryFile() as stderr:
        process=subprocess.Popen([str(root/bundle['entrypoint']),*bundle['arguments']],cwd=working,
            env={'PATH':'/usr/bin:/bin','KNOW_THINGS_ASSET_ROOT':str(root),'HF_HUB_OFFLINE':'1','TRANSFORMERS_OFFLINE':'1',
                 'HF_HUB_DISABLE_TELEMETRY':'1','PYTHONDONTWRITEBYTECODE':'1','TOKENIZERS_PARALLELISM':'false','OMP_NUM_THREADS':'2'},
            stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=stderr)
        try:
            hello=receive(process)
            assert hello['manifestId']==space and hello['runtime']==manifest['runtime'],hello
            responses=[]
            for request_id in [1,2]:
                request=dict(protocolVersion=1,requestId=request_id,operation='embed',text='Redis policy requires cache. '*args.repetitions,query=False)
                data=json.dumps(request).encode()
                process.stdin.write(struct.pack('>I',len(data))+data);process.stdin.flush()
                response=receive(process)
                assert response['status']=='ok' and response['requestId']==request_id,response
                if args.expected_input_tokens is not None:assert response['inputTokens']==args.expected_input_tokens,response
                if args.expected_windows is not None:assert response['windows']==args.expected_windows,response
                vector=response['vector']
                assert len(vector)==manifest['dimensions'] and all(math.isfinite(value) for value in vector)
                norm=math.sqrt(sum(value*value for value in vector));assert abs(norm-1)<1e-5
                responses.append(response)
            difference=max(abs(a-b) for a,b in zip(responses[0]['vector'],responses[1]['vector']))
            assert difference<1e-6,difference
            process.stdin.close();assert process.wait(timeout=5)==0
        finally:
            if process.poll() is None:process.kill();process.wait(timeout=5)
        stderr.seek(0);diagnostics=stderr.read().decode()
    report=dict(kind='bundled-embedding-window-protocol',embeddingBundleManifestSha256=digest(root/'bundle.json'),embeddingSpaceId=space,
        pooling=manifest['pooling'],fixtureOnly=True,actualCorpusAccessed=False,semanticQualityQualified=False,repeatedRequests=2,
        allPayloadTokens=responses[0]['inputTokens'],windows=responses[0].get('windows'),windowInputLimit=manifest['maxInputTokens'],
        dimensions=manifest['dimensions'],vectorNorm=norm,repeatMaxAbsoluteDifference=difference,repeatTolerance=1e-6,
        handshakeIdentityMatched=True,gracefulEofExit=True,semaphoreCleanupWarningOnGracefulEof='resource_tracker' in diagnostics,
        phraseRepetitions=args.repetitions,
        limitations=['Fixed synthetic text; no semantic-quality evidence','No generation, adversarial runtime, or supported hardware-tier qualification'])
    with args.output.open('x') as stream:json.dump(report,stream,indent=2);stream.write('\n')
    print(json.dumps(report))


if __name__=='__main__':main()
