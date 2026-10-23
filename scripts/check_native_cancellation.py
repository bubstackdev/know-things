#!/usr/bin/env python3
"""Cancel a real GPU answer after visible text, then verify a fresh answer's isolation."""
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
    for name in ['binary','embedding-bundle','flash-bundle','generation-profile','output']:parser.add_argument('--'+name,type=Path,required=True)
    args=parser.parse_args()
    log=args.output.with_suffix('.stderr');guard=args.output.with_suffix('.pressure.json')
    if any(path.exists() for path in [args.output,log,guard]):parser.error('Outputs must be new')
    if memory_pressure_level()!=1:raise RuntimeError('Defer GPU trial under memory pressure')
    binary=args.binary.resolve();embedding=args.embedding_bundle.resolve();flash=args.flash_bundle.resolve();profile=args.generation_profile.resolve()
    selected=json.loads((profile/'profile.json').read_text())
    options=['--embedding-bundle',str(embedding),'--flash-bundle',str(flash),'--generation-profile',str(profile)]
    settings=dict(profile=selected['name'],mode='hybrid',strictHybrid=True,evidenceTokenBudget=1200,inputTokenBudget=3840,outputTokenBudget=1024,deadlineMs=30000,queueDeadlineMs=30000)
    events=[];failure=None;baseline=None;recovered=None;cancel_ms=None
    args.output.parent.mkdir(parents=True,exist_ok=True)
    try:
        with tempfile.TemporaryDirectory(prefix='know-things-gpu-cancel-') as temporary,os.fdopen(os.open(log,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600),'w') as stderr:
            root=Path(temporary).resolve();source=root/'source';source.mkdir();raw='Cache policy: Redis is the required cache backend.\n';(source/'policy.md').write_text(raw)
            with LocalClient(binary,root/'data',options,cwd=root,stderr=stderr) as client,protect_owned_work(client,guard):
                def ok(**request):
                    response=client.call(timeout=180,**request);assert response['status']=='ok',response
                    return response['data']
                assert ok(operation='status')['generationAvailable']
                ok(operation='register',name='fixture',root=str(source));ok(operation='reconcile',collection='fixture')
                question='Which cache backend is required?'
                def answer():
                    trace=AnswerTrace({'policy.md':raw.encode()},settings)
                    client.call('answer',question=question,collections=['fixture'],**settings,progress=trace.observe,timeout=35)
                    assert trace.answer is not None,(trace.error,trace.events)
                    assert trace.answer['referenceValidation']=='valid'
                    return trace.answer
                baseline=answer()
                def send(operation,**fields):
                    client.serial+=1;identifier=str(client.serial)
                    view=memoryview((json.dumps(dict(schemaVersion=1,requestId=identifier,operation=operation,**fields))+'\n').encode())
                    deadline=time.monotonic()+3
                    while view:
                        client._ready(client.process.stdin,True,deadline)
                        try:count=os.write(client.process.stdin.fileno(),view)
                        except BlockingIOError:continue
                        if not count:raise RuntimeError('Owned coordinator closed input')
                        view=view[count:]
                    return identifier
                request=send('answer',question='Explain the required cache backend in detail, including what the policy does and does not specify.',collections=['fixture'],**settings)
                deadline=time.monotonic()+35;cancel_id=None;ack=False;terminal=None;started=None
                while terminal is None or not ack:
                    event=client._line(deadline);events.append(event)
                    if event['requestId']==request:
                        if ack:assert event.get('data',{}).get('event')!='answer_delta','Visible text crossed the cancellation acknowledgment fence'
                        if event['status']!='streaming':terminal=event
                        elif event['data']['event']=='answer_delta' and cancel_id is None:
                            started=time.monotonic();cancel_id=send('cancel',runId=event['data']['runId'])
                    elif event['requestId']==cancel_id:
                        assert event['status']=='ok',event
                        ack=True
                    else:raise AssertionError('Unexpected response ID')
                    if terminal is not None and cancel_id is None:raise AssertionError('Answer ended before active cancellation')
                cancel_ms=(time.monotonic()-started)*1000
                assert terminal['error']['code']=='canceled',terminal
                assert terminal['data']['workerStopped'] is True,terminal
                assert cancel_ms<3000,cancel_ms
                recovered=answer()
                assert baseline['text']==recovered['text'],'Prior canceled output contaminated the next greedy answer'
                assert baseline['profile']==recovered['profile'] and baseline['evidencePackId']==recovered['evidencePackId']
                for citation in recovered['citations']:assert ok(operation='context',reference=citation['reference'])['text']==raw
                ok(operation='integrity')
            assert client.process.returncode==0
            assert (source/'policy.md').read_text()==raw
    except Exception as error:
        failure=type(error).__name__
        raise
    finally:
        write_new(args.output,dict(kind='real-native-cancellation-isolation',binarySha256=digest(binary),scriptSha256=digest(Path(__file__)),pressureMonitorScriptSha256=digest(Path(__file__).with_name('pressure_monitor.py')),embeddingManifestSha256=digest(embedding/'bundle.json'),flashManifestSha256=digest(flash/'bundle.json'),generationManifestSha256=digest(profile/'bundle.json'),pressureReceiptSha256=digest(guard) if guard.exists() else None,cancellationMs=cancel_ms,baseline=baseline,recovered=recovered,events=events,failureType=failure,fixtureOnly=True,actualCorpusAccessed=False,semanticQualityQualified=False,releaseQualified=False))
    print(json.dumps(dict(activeCancellation='passed',cancellationMs=cancel_ms,nextGreedyAnswerMatched=True)))


if __name__=='__main__':main()
