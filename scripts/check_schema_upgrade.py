#!/usr/bin/env python3
"""Check installed schema upgrades and legacy-backup restore on disposable data."""
import argparse
import json
import sqlite3
import subprocess
import tempfile
from pathlib import Path
from local_client import LocalClient
from package_installation import digest


def state(data, legacy=False):
    # Called only after the owning child exited cleanly. The host SQLite reader
    # cannot open a WAL-mode database read-only when its sidecars are absent.
    # A checkpointed immutable snapshot needs no sidecar creation; never apply
    # immutable mode to a live/uncheckpointed database.
    assert not any((data/name).exists() for name in ('store.sqlite-wal','store.sqlite-shm'))
    connection=sqlite3.connect((data/'store.sqlite').as_uri()+'?mode=ro&immutable=1',uri=True)
    try:
        version=connection.execute('PRAGMA user_version').fetchone()[0]
        sql='SELECT chunk_id,space,data FROM vectors ORDER BY chunk_id,space' if legacy else 'SELECT v.chunk_id,v.space,e.data FROM vectors v JOIN vector_values e ON e.id=v.value_id ORDER BY v.chunk_id,v.space'
        rows=connection.execute(sql).fetchall()
        values=len(rows) if legacy else connection.execute('SELECT count(*) FROM vector_values').fetchone()[0]
        policies=connection.execute('SELECT collection_id,policy FROM collection_policies ORDER BY collection_id').fetchall()
        return version,rows,values,policies
    finally:connection.close()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('old-binary','binary','embedding-bundle','output'):
        parser.add_argument('--'+name,type=Path,required=True)
    parser.add_argument('--old-schema',type=int,choices=[4,5],default=4)
    parser.add_argument('--new-schema',type=int,choices=[5,6],default=5)
    args=parser.parse_args()
    if args.new_schema<=args.old_schema:parser.error('Select a strictly newer schema')
    if args.output.exists():parser.error('Output must be new')
    old,binary,bundle=args.old_binary.resolve(),args.binary.resolve(),args.embedding_bundle.resolve()
    options=['--embedding-bundle',str(bundle)]
    with tempfile.TemporaryDirectory(prefix='know-things-installed-upgrade-') as temporary:
        root=Path(temporary).resolve();data=root/'data';backup=root/'legacy-backup.sqlite';restored=root/'restored'
        raw='Redis policy requires cache.\n'
        sources=[]
        for name in ('first','second'):
            source=root/name;source.mkdir();(source/'guide.md').write_text(raw);sources.append(source)
        policy=dict(excludedPaths=['ignored'],reconcileIntervalMs=None,retention=dict(retainRevisions=2,intervalMs=86400000))
        search=dict(operation='search',query='Redis policy',collections=['first','second'],mode='hybrid',strictHybrid=True)
        def ok(client,**request):
            result=client.call(timeout=90,**request)
            assert result['status']=='ok',result
            return result
        with LocalClient(old,data,options,cwd=root) as client:
            for source in sources:
                ok(client,operation='register',name=source.name,root=str(source),policy=policy)
                imported=ok(client,operation='reconcile',collection=source.name)['data']
                assert imported['active']==1 and not imported['failures'],imported
            before=ok(client,**search)
            references=[e['reference'] for e in before['data']['evidence']]
            assert len(references)==2
            policies={name:ok(client,operation='collection_policy',collection=name)['data'] for name in ('first','second')}
            space=before['data']['embeddingSpaceId'];generation=before['corpusGeneration']
            ok(client,operation='backup',destination=str(backup))
        assert client.process.returncode==0,client.process.returncode
        legacy=state(data,legacy=args.old_schema<5)
        assert legacy[0]==args.old_schema and len(legacy[1])==2
        backup_hash=digest(backup)
        def verify(data):
            with LocalClient(binary,data,options,cwd=root) as client:
                after=ok(client,**search)
                assert after['corpusGeneration']==generation
                assert after['data']['embeddingSpaceId']==space
                assert after['data']['evidencePackId']==before['data']['evidencePackId']
                assert [e['reference'] for e in after['data']['evidence']]==references
                for reference in references:
                    assert ok(client,operation='context',reference=reference)['data']['text']==raw
                for name in ('first','second'):
                    expected=dict(policies[name])
                    if args.new_schema>=6:expected.setdefault('watch',False)
                    assert ok(client,operation='collection_policy',collection=name)['data']==expected
                assert ok(client,operation='integrity')['data']['vectors']=='ok'
            assert client.process.returncode==0,client.process.returncode
            current=state(data)
            assert current[0]==args.new_schema and current[1]==legacy[1] and current[2]==1 and current[3]==legacy[3]
            return current
        migrated=verify(data)
        subprocess.run([str(binary),'restore',str(backup),str(restored)],check=True,capture_output=True,text=True,timeout=30)
        verify(restored)
        assert digest(backup)==backup_hash
        refusal=subprocess.run([str(old),'local',str(data)],input='',capture_output=True,text=True,timeout=10)
        assert refusal.returncode==1 and not refusal.stdout and json.loads(refusal.stderr)['code']=='store_incompatible',refusal
        assert state(data)==migrated
        assert all((source/'guide.md').read_text()==raw for source in sources)
    report=dict(kind='installed-schema-upgrade',oldSchema=args.old_schema,newSchema=args.new_schema,oldBinarySha256=digest(old),binarySha256=digest(binary),
        embeddingBundleManifestSha256=digest(bundle/'bundle.json'),embeddingSpaceId=space,
        fixtureOnly=True,actualCorpusAccessed=False,semanticQualityQualified=False,
        preserved=['packed vector bytes','exact references','evidence pack ID','corpus generation','collection policies','active embedding space'],
        legacyChunkMappings=2,oldStoredValues=legacy[2],sharedValuesAfterUpgrade=1,legacyBackupUnchanged=True,legacyBackupRestored=True,
        olderBinaryRefusedNewSchema=True,sourceUnchanged=True,
        independentDatabaseRead='checkpointed immutable snapshots after clean owned-core exit',
        limitations=['Disposable fixture and captured CPU runtime only','No trained-model quality, generation, or hardware-tier qualification','Migration capacity rollback is separately tested with a private SQLite pager limit'])
    with args.output.open('x') as stream:json.dump(report,stream,indent=2);stream.write('\n')
    print(json.dumps(dict(installedSchemaUpgrade='passed',oldSchema=args.old_schema,newSchema=args.new_schema)))


if __name__=='__main__':main()
