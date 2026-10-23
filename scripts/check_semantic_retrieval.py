#!/usr/bin/env python3
"""One authored semantic-only retrieval diagnostic; not a corpus quality benchmark."""
import argparse
import json
import tempfile
from pathlib import Path
from local_client import LocalClient
from package_installation import digest


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('binary','embedding-bundle','output'):parser.add_argument('--'+name,type=Path,required=True)
    args=parser.parse_args()
    if args.output.exists():parser.error('Output must be new')
    binary=args.binary.resolve();bundle=args.embedding_bundle.resolve()
    documents={'cars.md':'Electric cars use battery power to drive their motors.\n',
        'databases.md':'Relational databases organize records into tables.\n',
        'bread.md':'Bread is made by baking flour, water and yeast.\n'}
    with tempfile.TemporaryDirectory(prefix='know-things-semantic-check-') as temporary:
        root=Path(temporary).resolve();source=root/'source';source.mkdir()
        for name,text in documents.items():(source/name).write_text(text)
        with LocalClient(binary,root/'data',['--embedding-bundle',str(bundle)],cwd=root) as client:
            def ok(**request):
                response=client.call(timeout=90,**request);assert response['status']=='ok',response
                return response['data']
            ok(operation='register',name='fixture',root=str(source))
            imported=ok(operation='reconcile',collection='fixture');assert imported['active']==3 and not imported['failures'],imported
            query=dict(operation='search',query='automobile propulsion',collections=['fixture'],topK=1)
            lexical=ok(**query,mode='lexical');assert lexical['evidence']==[],lexical
            hybrid=ok(**query,mode='hybrid',strictHybrid=True)
            assert hybrid['actualMode']=='hybrid' and hybrid['embeddingSpaceId'],hybrid
            first=hybrid['evidence'][0];assert first['source']=='cars.md',first
            assert ok(operation='context',reference=first['reference'])['text']==documents['cars.md']
            ok(operation='integrity')
        assert client.process.returncode==0
        assert all((source/name).read_text()==text for name,text in documents.items())
    report=dict(kind='authored-semantic-only-retrieval-diagnostic',binarySha256=digest(binary),scriptSha256=digest(Path(__file__)),
        embeddingBundleManifestSha256=digest(bundle/'bundle.json'),embeddingSpaceId=hybrid['embeddingSpaceId'],
        lexicalEvidenceCount=0,hybridFirstSource='cars.md',semanticOnlyFixturePassed=True,caseCount=1,
        fixtureOnly=True,actualCorpusAccessed=False,semanticQualityQualified=False,
        limitations=['One authored three-document case; no held-out or human-reviewed retrieval metric','No general evidence-recall, competitive, or latency qualification'])
    with args.output.open('x') as stream:json.dump(report,stream,indent=2);stream.write('\n')
    print(json.dumps(dict(semanticOnlyFixture='passed',semanticQualityQualified=False)))


if __name__=='__main__':main()
