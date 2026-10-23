#!/usr/bin/env python3
"""Check owned coordinator/embedding-worker shutdown and restart on fixture data."""
import argparse
import json
import tempfile
import time
from pathlib import Path
from local_client import LocalClient
from package_installation import digest


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--embedding-bundle',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--use-packaged-assets',action='store_true')
    args=parser.parse_args()
    if args.output.exists():parser.error('Output must be new')
    binary=args.binary.resolve();bundle=args.embedding_bundle.resolve()
    options=[] if args.use_packaged_assets else ['--embedding-bundle',str(bundle)]
    cycles=[]
    with tempfile.TemporaryDirectory(prefix='know-things-worker-shutdown-') as temporary:
        root=Path(temporary).resolve();source=root/'source';source.mkdir()
        raw='Redis policy requires cache.\n';(source/'guide.md').write_text(raw)
        space=None
        for index in range(2):
            with tempfile.TemporaryFile() as stderr:
                client=LocalClient(binary,root/'data',options,cwd=root,stderr=stderr)
                try:
                    for request in [dict(operation='register',name='fixture',root=str(source)),dict(operation='reconcile',collection='fixture')]:
                        response=client.call(timeout=90,**request)
                        assert response['status']=='ok',response
                    response=client.call(operation='search',query='Redis policy',collections=['fixture'],mode='hybrid',strictHybrid=True)
                    assert response['status']=='ok' and response['data']['actualMode']=='hybrid',response
                    assert response['data']['evidence']
                    current=response['data']['embeddingSpaceId'];assert current
                    if space is not None:assert current==space
                    space=current
                finally:
                    started=time.monotonic();client.close();elapsed=(time.monotonic()-started)*1000
                assert client.process.returncode==0,client.process.returncode
                stderr.seek(0);diagnostics=stderr.read().decode()
                tracker_warning='resource_tracker' in diagnostics or 'leaked semaphore' in diagnostics
                assert not tracker_warning,'Owned embedding worker still emits resource-tracker cleanup warnings'
                cycles.append(dict(cycle=index+1,coordinatorExitCode=client.process.returncode,shutdownMs=elapsed,semaphoreTrackerWarning=False))
        assert (source/'guide.md').read_text()==raw
    report=dict(kind='owned-embedding-worker-shutdown',binarySha256=digest(binary),embeddingBundleManifestSha256=digest(bundle/'bundle.json'),
        embeddingSpaceId=space,cycles=cycles,fixtureOnly=True,actualCorpusAccessed=False,semanticQualityQualified=False,
        limitations=['Observed captured CPU runtime and fixture only','No real-model, GPU, system-pressure, or universal shutdown guarantee'])
    with args.output.open('x') as stream:json.dump(report,stream,indent=2);stream.write('\n')
    print(json.dumps(report))


if __name__=='__main__':main()
