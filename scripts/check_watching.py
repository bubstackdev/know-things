#!/usr/bin/env python3
"""Exercise native source hints on disposable documents; never mutate the actual corpus."""
import argparse
import json
import subprocess
import tempfile
import time
from pathlib import Path
from local_client import LocalClient
from package_installation import digest


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('binary','output'):parser.add_argument('--'+name,type=Path,required=True)
    parser.add_argument('--embedding-bundle',type=Path)
    parser.add_argument('--use-packaged-assets',action='store_true')
    args=parser.parse_args()
    if args.output.exists():parser.error('Output must be new')
    binary=args.binary.resolve();bundle=args.embedding_bundle.resolve() if args.embedding_bundle else None
    options=['--embedding-bundle',str(bundle)] if bundle and not args.use_packaged_assets else []
    observations=[];startups=[]
    with tempfile.TemporaryDirectory(prefix='know-things-native-watch-') as temporary:
        root=Path(temporary).resolve();source=root/'source';source.mkdir();data=root/'data'
        guide=source/'guide.md';guide.write_text('initial policy\n')
        (source/'private').mkdir();(source/'private/invalid.txt').write_bytes(b'\xff')
        def ok(client,**request):
            response=client.call(timeout=90,**request);assert response['status']=='ok',response
            return response['data']
        def search(client,word):return ok(client,operation='search',query=word,collections=['fixture'],mode='lexical')['evidence']
        def status(client):return ok(client,operation='status')['periodicReconciliation']
        def ready(client,phase):
            started=time.monotonic();ok(client,operation='status')
            startups.append(dict(phase=phase,firstStatusMs=(time.monotonic()-started)*1000))
        def wait(predicate):
            deadline=time.monotonic()+10
            while not predicate():
                if time.monotonic()>=deadline:
                    raise AssertionError(json.dumps(dict(error='Native watch observation timed out before safety scan',maintenance=status(client))))
                time.sleep(.025)
        def events(client):return status(client)['watch']['observedEvents']
        with LocalClient(binary,data,options,cwd=root) as client:
            ready(client,'initial')
            identifier=ok(client,operation='register',name='fixture',root=str(source),policy=dict(watch=True,reconcileIntervalMs=86400000,excludedPaths=['private']))['collectionId']
            wait(lambda:bool(search(client,'initial')))
            metadata=status(client)['watch'];assert metadata['backend']=='kqueue-vnode' and metadata['collections'][identifier]['active'],metadata
            assert metadata['safetyScanMaxIntervalMs']==60000
            original=search(client,'initial')[0]['reference']
            def action(name,mutate,predicate):
                before=events(client);started=time.monotonic();mutate()
                wait(lambda:events(client)>before and predicate() and status(client)['activeCollection'] is None)
                observations.append(dict(action=name,observedMs=(time.monotonic()-started)*1000,nativeEventObserved=True))
            action('in-place edit',lambda:guide.write_text('edited policy\n'),lambda:bool(search(client,'edited')))
            assert ok(client,operation='context',reference=original)['text']=='initial policy\n'
            def replace():
                staged=source/'replacement.tmp';staged.write_text('replacement policy\n');staged.replace(guide)
            action('atomic replacement',replace,lambda:bool(search(client,'replacement')))
            def nested():
                (source/'nested').mkdir();(source/'nested/new.md').write_text('nested policy\n')
            action('nested creation',nested,lambda:bool(search(client,'nested')))
            action('rename',lambda:guide.rename(source/'renamed.md'),lambda:any(e['source']=='renamed.md' for e in search(client,'replacement')))
            unavailable=client.call(operation='context',reference=original)
            assert unavailable['status']=='error' and unavailable['error']['code']=='source_unavailable',unavailable
            action('deletion',lambda:(source/'renamed.md').unlink(),lambda:not search(client,'replacement'))
            pinned=search(client,'nested')[0]['reference']
            backup=root/'backup.sqlite';ok(client,operation='backup',destination=str(backup))
            assert status(client)['collections'][identifier]['lastReport']['failures']==0
        assert client.process.returncode==0
        (source/'nested/new.md').write_text('offline policy\n')
        with LocalClient(binary,data,options,cwd=root) as client:
            ready(client,'restart')
            wait(lambda:bool(search(client,'offline')))
            assert ok(client,operation='context',reference=pinned)['text']=='nested policy\n'
            ok(client,operation='set_collection_policy',collection='fixture',policy=dict(excludedPaths=['private']))
            wait(lambda:status(client)['configuredCollections']==0 and not status(client)['watch']['collections'])
            (source/'nested/new.md').write_text('disabled policy\n');time.sleep(.7)
            assert not search(client,'disabled')
            ok(client,operation='reconcile',collection='fixture');assert search(client,'disabled')
            ok(client,operation='integrity')
        assert client.process.returncode==0
        restored=root/'restored'
        subprocess.run([str(binary),'restore',str(backup),str(restored)],check=True,capture_output=True,text=True,timeout=30)
        with LocalClient(binary,restored,options,cwd=root) as client:
            ready(client,'restored')
            assert ok(client,operation='collection_policy',collection='fixture')['watch']
            wait(lambda:bool(search(client,'disabled')))
            assert ok(client,operation='context',reference=pinned)['text']=='nested policy\n'
            ok(client,operation='integrity')
            space=None
            if bundle:
                pack=ok(client,operation='search',query='policy',collections=['fixture'],mode='hybrid',strictHybrid=True)
                assert pack['actualMode']=='hybrid' and pack['embeddingSpaceId'];space=pack['embeddingSpaceId']
        assert client.process.returncode==0
        messages=[dict(jsonrpc='2.0',id=1,method='initialize',params=dict(protocolVersion='2025-06-18',capabilities={},clientInfo=dict(name='watch-fixture',version='1'))),
            dict(jsonrpc='2.0',method='notifications/initialized'),
            dict(jsonrpc='2.0',id=2,method='tools/call',params=dict(name='know_things',arguments=dict(operation='collection_policy',collection='fixture'))),
            dict(jsonrpc='2.0',id=3,method='tools/call',params=dict(name='know_things',arguments=dict(operation='context',reference=pinned)))]
        result=subprocess.run([str(binary),'mcp-stdio',str(restored),*options],cwd=root,input=''.join(json.dumps(m)+'\n' for m in messages),capture_output=True,text=True,timeout=90,check=True)
        responses={value['id']:value for value in map(json.loads,result.stdout.splitlines())}
        for number in [2,3]:assert not responses[number]['result']['isError'],responses[number]
        assert json.loads(responses[2]['result']['content'][0]['text'])['data']['watch']
        assert json.loads(responses[3]['result']['content'][0]['text'])['data']['text']=='nested policy\n'
    report=dict(kind='native-watching-operational-smoke',binarySha256=digest(binary),scriptSha256=digest(Path(__file__)),
        embeddingBundleManifestSha256=digest(bundle/'bundle.json') if bundle else None,embeddingSpaceId=space,
        fixtureOnly=True,actualCorpusAccessed=False,semanticQualityQualified=False,backend='kqueue-vnode',
        observations=observations,startupDiagnostics=startups,safetyScanMaxIntervalMs=60000,packagedAssetsSelected=args.use_packaged_assets,
        workflows=['native source changes','exact pinned context','exclusion filters','offline edits after restart','disable/manual reconcile','backup/restore with watch policy','MCP policy/context','clean owned-core exits'],
        limitations=['Small disposable fixture; no actual-corpus or supported-hardware qualification','Native target-budget failure and manual-ingestion contention are separately covered in Rust tests','Startup is separately recorded and excluded from watch observation deadlines','Diagnostic detection times include client polling; no model-quality, ingestion p95, or independent network-denial claim'])
    with args.output.open('x') as stream:json.dump(report,stream,indent=2);stream.write('\n')
    print(json.dumps(dict(nativeWatching='passed',changes=len(observations),semanticQualityQualified=False)))


if __name__=='__main__':main()
