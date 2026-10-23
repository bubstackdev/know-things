#!/usr/bin/env python3
"""Run sequential cited-answer/reset diagnostics with explicitly selected real local assets."""
import argparse
import json
import os
import tempfile
import time
from pathlib import Path
from evaluate_answers import AnswerTrace,write_new
from local_client import LocalClient
from package_installation import digest
from pressure_monitor import memory_pressure_level,protect_owned_work



def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('binary','embedding-bundle','flash-bundle','generation-profile','output'):
        parser.add_argument('--'+name,type=Path,required=True)
    parser.add_argument('--startup-deadline-ms',type=int,default=600000)
    parser.add_argument('--use-packaged-assets',action='store_true')
    args=parser.parse_args()
    if not 1000<=args.startup_deadline_ms<=900000:parser.error('Startup deadline must be 1000–900000 ms')
    if args.output.exists():parser.error('Output must be new')
    binary=args.binary.resolve();embedding=args.embedding_bundle.resolve();flash=args.flash_bundle.resolve();profile=args.generation_profile.resolve()
    selected=json.loads((profile/'profile.json').read_text())
    options=['--embedding-bundle',str(embedding),'--flash-bundle',str(flash),'--generation-profile',str(profile),'--generation-startup-deadline-ms',str(args.startup_deadline_ms)]
    installation=None
    if args.use_packaged_assets:
        package=binary.parent.parent
        installation=json.loads((package/'bundle.json').read_text())
        if installation['kind']!='installation' or binary!=package/'bin/know-things':
            parser.error('Packaged assets require the installation launcher')
        assets=installation['manifest']['assets']
        if any((package/assets[key]).resolve()!=selected for key,selected in [('embedding',embedding),('flash',flash),('generationProfile',profile)]):
            parser.error('Selected manifests must match the installation assets')
        options=['--generation-startup-deadline-ms',str(args.startup_deadline_ms)]
    log=args.output.with_suffix('.stderr');log.parent.mkdir(parents=True,exist_ok=True)
    cases=[]
    traces=[];status=None;startup_ms=None
    if memory_pressure_level()!=1:raise RuntimeError('Defer GPU trial until system memory pressure is normal')
    pressure_receipt=args.output.with_suffix('.pressure.json')
    if pressure_receipt.exists():parser.error('Pressure receipt must be new')
    try:
        with tempfile.TemporaryDirectory(prefix='know-things-native-answers-') as temporary,os.fdopen(os.open(log,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600),'w') as stderr:
            root=Path(temporary).resolve();source=root/'source';source.mkdir()
            raw='Cache policy: Redis is the required cache backend. Database policy: PostgreSQL is required for durable storage.\n'
            (source/'policy.md').write_text(raw)
            startup=time.monotonic()
            with LocalClient(binary,root/'data',options,cwd=root,stderr=stderr) as client,protect_owned_work(client,pressure_receipt):
                print(json.dumps(dict(stage='owned_core_started',pid=client.process.pid)),flush=True)
                def ok(timeout=180,**request):
                    response=client.call(timeout=timeout,**request);assert response['status']=='ok',response
                    return response['data']
                status=ok(operation='status',timeout=args.startup_deadline_ms/1000+60);startup_ms=(time.monotonic()-startup)*1000
                print(json.dumps(dict(stage='model_ready',startupMs=startup_ms,generationAvailable=status['generationAvailable'],generationError=status['generationError'])),flush=True)
                assert status['generationAvailable'] and status['generationError'] is None,status['generationError']
                assert status['embeddingError'] is None,status['embeddingError']
                ok(operation='register',name='fixture',root=str(source))
                imported=ok(operation='reconcile',collection='fixture');assert imported['active']==1 and not imported['failures'],imported
                settings=dict(profile=selected['name'],mode='hybrid',strictHybrid=True,evidenceTokenBudget=1200,inputTokenBudget=3840,outputTokenBudget=160,deadlineMs=30000,queueDeadlineMs=30000,queuePolicy='reject')
                for question,required in [('Which cache backend is required?',['redis']),('Which database is required for durable storage?',['postgresql','postgres']),('Which cache backend is required?',['redis'])]:
                    trace=AnswerTrace({'policy.md':raw.encode()},settings)
                    client.call('answer',question=question,collections=['fixture'],**settings,progress=trace.observe,timeout=35)
                    traces.append(trace.events)
                    assert trace.answer is not None,(trace.error,trace.events[-1] if trace.events else None)
                    answer=trace.answer
                    assert any(word in answer['text'].lower() for word in required),answer
                    assert answer['referenceValidation']=='valid' and answer['citations'],answer
                    for citation in answer['citations']:
                        assert ok(operation='context',reference=citation['reference'])['text']==raw
                    cases.append(dict(text=answer['text'],profile=answer['profile'],embeddingSpaceId=answer['embeddingSpaceId'],evidencePackId=answer['evidencePackId'],generatedTokens=answer['generatedTokens'],visibleTokens=answer['visibleTokens'],hiddenTokens=answer['hiddenTokens'],timingsMs=answer['timingsMs'],clientFirstVisibleMs=trace.first_visible_ms,clientTerminalMs=trace.terminal_ms,referenceValidation=answer['referenceValidation'],semanticValidation=answer['semanticValidation']))
                    print(json.dumps(dict(stage='answer',case=len(cases),text=answer['text'],terminalMs=trace.terminal_ms)),flush=True)
                assert cases[0]['text']==cases[2]['text'],'Greedy repeat differs after an intervening request'
                ok(operation='integrity')
            assert client.process.returncode==0,client.process.returncode
            assert (source/'policy.md').read_text()==raw
    except Exception as error:
        write_new(args.output,dict(kind='real-native-cited-answer-failed-diagnostic',binarySha256=digest(binary),scriptSha256=digest(Path(__file__)),embeddingBundleManifestSha256=digest(embedding/'bundle.json'),flashBundleManifestSha256=digest(flash/'bundle.json'),generationProfileManifestSha256=digest(profile/'bundle.json'),startupMs=startup_ms,startupDeadlineMs=args.startup_deadline_ms,generationStatus=status,failureType=type(error).__name__,cases=cases,traces=traces,stderrSha256=digest(log),pressureMonitorReceiptSha256=digest(pressure_receipt) if pressure_receipt.exists() else None,fixtureOnly=True,actualCorpusAccessed=False,semanticQualityQualified=False,releaseQualified=False))
        raise
    report=dict(kind='real-native-cited-answer-diagnostics',binarySha256=digest(binary),scriptSha256=digest(Path(__file__)),embeddingBundleManifestSha256=digest(embedding/'bundle.json'),flashBundleManifestSha256=digest(flash/'bundle.json'),generationProfileManifestSha256=digest(profile/'bundle.json'),
        startupMs=startup_ms,startupDeadlineMs=args.startup_deadline_ms,pressureMonitorReceiptSha256=digest(pressure_receipt),cases=cases,greedyRepeatMatched=True,fixtureOnly=True,actualCorpusAccessed=False,realGenerationExecuted=True,semanticQualityQualified=False,releaseQualified=False,
        packagedAssetsSelected=args.use_packaged_assets,installationManifestSha256=digest(binary.parent.parent/'bundle.json') if installation else None,
        limitations=['Three authored-fixture requests; no held-out or human-calibrated answer-quality evidence','Literal expected words and valid source references do not prove general factual correctness','No active cancellation/crash/GPU-pressure qualification in this script','Shared workstation; no other sessions/processes stopped or isolated'])
    write_new(args.output,report)
    print(json.dumps(dict(nativeCitedAnswers='passed',requests=len(cases),semanticQualityQualified=False)))


if __name__=='__main__':main()
