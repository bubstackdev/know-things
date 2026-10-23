#!/usr/bin/env python3
"""Verify an explicit local embedding bundle operationally on disposable documents.

This does not measure semantic quality, even if the supplied bundle is trained.
It never reads the actual corpus. Output must be new.
"""
import argparse
import json
import sqlite3
import subprocess
import tempfile
import time
from pathlib import Path
from local_client import LocalClient
from package_installation import digest


def storage(data):
    connection=sqlite3.connect((data/'store.sqlite').as_uri()+'?mode=ro',uri=True)
    try:
        links,values=connection.execute('SELECT (SELECT count(*) FROM vectors),(SELECT count(*) FROM vector_values)').fetchone()
        return dict(chunkMappings=links,sharedValues=values)
    finally:connection.close()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--embedding-bundle',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--use-packaged-assets',action='store_true',help='Let the installation launcher select its included assets')
    args=parser.parse_args()
    binary=args.binary.resolve();bundle=args.embedding_bundle.resolve()
    if args.output.exists():parser.error('Output must be new')
    options=[] if args.use_packaged_assets else ['--embedding-bundle',str(bundle)]
    timings=[];shared_storage={}
    with tempfile.TemporaryDirectory(prefix='know-things-bundled-embedding-smoke-') as temporary:
        root=Path(temporary).resolve();source=root/'source';source.mkdir()
        initial='Redis policy requires cache. '*20+'\n';replacement='Redis documentation policy.\n'
        (source/'guide.md').write_text(initial)
        duplicate=root/'duplicate';duplicate.mkdir();(duplicate/'copy.md').write_text(initial)
        data=root/'data';restored=root/'restored';backup=root/'backup.sqlite'
        with LocalClient(binary,data,options,cwd=root) as client:
            def call(**request):
                result=client.call(timeout=90,**request)
                assert result['status']=='ok',result
                return result
            status=call(operation='status')['data']
            space=status['configuredEmbeddingSpaceId'];assert space
            call(operation='register',name='fixture',root=str(source))
            imported=call(operation='reconcile',collection='fixture')['data']
            assert imported['active']==1 and not imported['failures'],imported
            for _ in range(5):
                start=time.monotonic()
                result=call(operation='search',query='Redis policy',collections=['fixture'],mode='hybrid',strictHybrid=True)
                pack=result['data'];assert pack['actualMode']=='hybrid' and pack['embeddingSpaceId']==space,pack
                assert pack['evidence'] and not pack['degradationReasons']
                timings.append(dict(clientMs=(time.monotonic()-start)*1000,coordinatorMs=result['durationMs'],stagesMs=pack['timingsMs']))
            old=pack['evidence'][0]['reference'];old_text=pack['evidence'][0]['text']
            assert call(operation='context',reference=old)['data']['text']==old_text
            call(operation='register',name='duplicate',root=str(duplicate))
            assert call(operation='reconcile',collection='duplicate')['data']['active']==1
            shared_storage['afterDuplicateImport']=storage(data)
            assert shared_storage['afterDuplicateImport']==dict(chunkMappings=2,sharedValues=1)
            copied=call(operation='search',query='Redis policy',collections=['duplicate'],mode='hybrid',strictHybrid=True)['data']['evidence'][0]
            assert copied['reference']['chunkId']!=old['chunkId'] and copied['text']==old_text
            (source/'guide.md').write_text(replacement)
            assert call(operation='reconcile',collection='fixture')['data']['active']==1
            assert call(operation='context',reference=old)['data']['text']==old_text
            shared_storage['afterEdit']=storage(data)
            assert shared_storage['afterEdit']==dict(chunkMappings=3,sharedValues=2)
            call(operation='exclude',document=old['documentId'])
            unavailable=client.call(operation='context',reference=old)
            assert unavailable['status']=='error' and unavailable['error']['code']=='source_unavailable',unavailable
            call(operation='include',document=old['documentId'])
            call(operation='reconcile',collection='fixture')
            call(operation='prune',retainRevisions=1,maxRevisions=1)
            unavailable=client.call(operation='context',reference=old)
            assert unavailable['status']=='error' and unavailable['error']['code']=='evidence_version_unavailable',unavailable
            assert call(operation='context',reference=copied['reference'])['data']['text']==old_text
            shared_storage['afterPrune']=storage(data)
            assert shared_storage['afterPrune']==dict(chunkMappings=2,sharedValues=2)
            pack=call(operation='search',query='Redis policy',collections=['fixture'],mode='hybrid',strictHybrid=True)['data']
            current=pack['evidence'][0]['reference'];current_text=pack['evidence'][0]['text']
            assert call(operation='integrity')['data']['sqlite']=='ok'
            call(operation='backup',destination=str(backup))
        subprocess.run([str(binary),'restore',str(backup),str(restored)],check=True,capture_output=True,text=True,timeout=30)
        with LocalClient(binary,restored,options,cwd=root) as client:
            result=client.call(operation='context',reference=current)
            assert result['status']=='ok' and result['data']['text']==current_text,result
            result=client.call(operation='search',query='Redis policy',collections=['fixture'],mode='hybrid',strictHybrid=True)
            assert result['status']=='ok' and result['data']['embeddingSpaceId']==space,result
            assert client.call(operation='integrity')['status']=='ok'
        messages=[dict(jsonrpc='2.0',id=1,method='initialize',params=dict(protocolVersion='2025-06-18',capabilities={},clientInfo=dict(name='embedding-smoke',version='1'))),
            dict(jsonrpc='2.0',method='notifications/initialized'),
            dict(jsonrpc='2.0',id=2,method='tools/call',params=dict(name='know_things',arguments=dict(operation='search',query='Redis policy',collections=['fixture'],mode='hybrid',strictHybrid=True)))]
        result=subprocess.run([str(binary),'mcp-stdio',str(restored),*options],cwd=root,
            input=''.join(json.dumps(message)+'\n' for message in messages),capture_output=True,text=True,timeout=90)
        assert result.returncode==0,result.stderr
        responses=[json.loads(line) for line in result.stdout.splitlines()]
        response=next(value for value in responses if value['id']==2)
        assert not response['result']['isError'],response
        result=json.loads(response['result']['content'][0]['text'])
        assert result['status']=='ok' and result['data']['actualMode']=='hybrid' and result['data']['embeddingSpaceId']==space,result
        with LocalClient(binary,restored,options,cwd=root) as client:
            assert client.call(operation='remove_collection',collection='fixture')['status']=='ok'
            assert client.call(operation='context',reference=copied['reference'])['data']['text']==old_text
            shared_storage['afterOriginalCollectionRemoval']=storage(restored)
            assert shared_storage['afterOriginalCollectionRemoval']==dict(chunkMappings=1,sharedValues=1)
            assert client.call(operation='remove_collection',collection='duplicate')['status']=='ok'
            shared_storage['afterLastOwnerRemoval']=storage(restored)
            assert shared_storage['afterLastOwnerRemoval']==dict(chunkMappings=0,sharedValues=0)
            assert client.call(operation='integrity')['status']=='ok'
        (source/'guide.md').write_text(initial)
    report=dict(kind='local-embedding-operational-smoke',binarySha256=digest(binary),
        embeddingBundleManifestSha256=digest(bundle/'bundle.json'),embeddingSpaceId=space,
        runtimeBundleManifestSha256=digest(bundle/'runtime/bundle.json') if (bundle/'runtime/bundle.json').is_file() else None,
        fixtureOnly=True,actualCorpusAccessed=False,semanticQualityQualified=False,packagedAssetsSelected=args.use_packaged_assets,
        workflows=['worker startup','strict hybrid import/query','shared vector values with independent source ownership','exact pinned context after edit','exclude/include','bounded pruning','integrity','backup/restore','MCP strict hybrid query','remove collection'],
        sharedVectorStorage=shared_storage,
        diagnosticQueries=timings,limitations=['Five disposable-fixture queries are not a latency or semantic acceptance benchmark','OS network-deny isolation is not imposed by this harness','No generation or human-reviewed label evaluation','Storage sharing is observed; inference-call reuse is separately covered by counting fixtures'])
    with args.output.open('x') as stream:json.dump(report,stream,indent=2);stream.write('\n')
    print(json.dumps(dict(operationalEmbeddingSmoke='passed',output=str(args.output),semanticQualityQualified=False)))


if __name__=='__main__':main()
