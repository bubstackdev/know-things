#!/usr/bin/env python3
"""Capture private, revision-pinned local answer traces for separately declared source review."""
import argparse
import json
import math
import os
import platform
import time
from pathlib import Path
from contextlib import nullcontext
from pressure_monitor import memory_pressure_level,protect_owned_work
from evaluate_retrieval import digest, snapshot, validate_suite
from local_client import LocalClient
from citations import citation_occurrences


def write_new(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    # An explicit capture contains questions, answers, and evidence. Never inherit
    # a permissive umask or replace a previous experiment/review.
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'w') as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())


def finite_nonnegative(value):
    return type(value) in (int, float) and math.isfinite(value) and value >= 0


class AnswerTrace:
    """Validate the observed stream without treating valid citations as entailment."""
    def __init__(self, files, settings, clock=time.monotonic):
        self.files, self.settings, self.clock = files, settings, clock
        self.started = clock()
        self.events = []
        self.pack = self.answer = self.error = self.run_id = None
        self.pieces = []
        self.first_visible_ms = self.terminal_ms = None

    def observe(self, response):
        if self.terminal_ms is not None:
            raise ValueError('Event after terminal response')
        elapsed = (self.clock()-self.started)*1000
        payload = response.get('data')
        if not payload:
            if self.events or response.get('status') != 'error' or not response.get('error', {}).get('code'):
                raise ValueError('Malformed answer admission failure')
            self.error = response['error']['code']
            self.terminal_ms = elapsed
            self.events.append(response)
            return
        event = payload.get('event')
        if payload.get('sequence') != len(self.events)+1 or not payload.get('runId'):
            raise ValueError('Invalid answer sequence/run identity')
        if not self.events:
            if event != 'accepted':
                raise ValueError('Answer stream must begin with acceptance')
            self.run_id = payload['runId']
        if payload['runId'] != self.run_id:
            raise ValueError('Answer run identity changed')
        terminal = event in ('completed', 'failed')
        expected_status = 'error' if event == 'failed' else 'ok' if terminal else 'streaming'
        if response.get('status') != expected_status:
            raise ValueError('Answer status/event mismatch')
        if event == 'accepted':
            if self.events:
                raise ValueError('Duplicate acceptance')
        elif event == 'evidence_ready':
            if self.pack is not None or self.pieces:
                raise ValueError('Duplicate or late evidence pack')
            self.pack = payload['pack']
            self.verify_evidence()
        elif event == 'answer_delta':
            if self.pack is None or not isinstance(payload.get('text'), str):
                raise ValueError('Answer delta without evidence')
            self.pieces.append(payload['text'])
            if payload['text'].strip() and self.first_visible_ms is None:
                self.first_visible_ms = elapsed
            if len(''.join(self.pieces).encode()) > 128*1024:
                raise ValueError('Answer text exceeds coordinator bound')
        elif event == 'completed':
            self.answer = payload['answer']
            self.verify_answer()
        elif event == 'failed':
            self.error = payload['error']['code']
            if response.get('error', {}).get('code') != self.error:
                raise ValueError('Inconsistent terminal error')
        else:
            raise ValueError('Unknown answer event')
        self.events.append(response)
        if terminal:
            self.terminal_ms = elapsed

    def verify_evidence(self):
        pack = self.pack
        if (type(pack.get('tokenCount')) is not int or not 0 <= pack['tokenCount'] <= self.settings['evidenceTokenBudget']
                or not pack.get('budgetTokenizer') or len(pack['evidence']) > 20):
            raise ValueError('Invalid answer evidence budget')
        if self.settings['strictHybrid'] and self.settings['mode'] == 'hybrid' and pack['actualMode'] != 'hybrid':
            raise ValueError('Strict hybrid silently degraded')
        for evidence in pack['evidence']:
            reference = evidence['reference']
            raw = self.files[evidence['source']]
            start, end = reference['byteStart'], reference['byteEnd']
            if (type(start) is not int or type(end) is not int or not 0 <= start < end <= len(raw)
                    or reference['contentHash'] != digest(raw)
                    or raw[start:end] != evidence['text'].encode()):
                raise ValueError('Answer evidence is not exact in the pinned snapshot')

    def verify_answer(self):
        answer, pack = self.answer, self.pack
        if pack is None or answer['text'] != ''.join(self.pieces) or not answer['text'].strip():
            raise ValueError('Completed answer differs from delivered deltas')
        for answer_key, pack_key in [('evidencePackId', 'evidencePackId'), ('corpusGeneration', 'corpusGeneration'),
                ('actualMode', 'actualMode'), ('embeddingSpaceId', 'embeddingSpaceId'),
                ('evidenceTokens', 'tokenCount'), ('degradationReasons', 'degradationReasons')]:
            if answer[answer_key] != pack[pack_key]:
                raise ValueError('Completed answer/evidence identity mismatch')
        if answer['profile']['tokenizerId'] != pack['budgetTokenizer']:
            raise ValueError('Answer tokenizer identity mismatch')
        for name in ['generatedTokens', 'visibleTokens', 'hiddenTokens', 'promptTokens']:
            if type(answer[name]) is not int or answer[name] < 0:
                raise ValueError('Invalid answer token metrics')
        policy = answer['visibleTokenCountKind'] == 'tokenized_policy_text'
        if policy:
            valid_counts = (answer['generatedTokens'] == answer['hiddenTokens'] == 0
                and answer['finishReason'] == 'insufficient_evidence' and not pack['evidence'])
        else:
            valid_counts = (answer['visibleTokenCountKind'] == 'generated'
                and answer['generatedTokens'] == answer['visibleTokens']+answer['hiddenTokens']
                and answer['finishReason'] == 'stop')
        if (not valid_counts or answer['generatedTokens'] > self.settings['outputTokenBudget']
                or answer['visibleTokens'] > self.settings['outputTokenBudget']
                or answer['promptTokens'] > self.settings['inputTokenBudget']
                or answer['promptTokens']+self.settings['outputTokenBudget'] > answer['profile']['contextLimit']):
            raise ValueError('Completed answer violates token/finish contract')
        if any(not finite_nonnegative(v) for v in answer['timingsMs'].values()):
            raise ValueError('Non-finite or negative stage timing')
        labels = set(citation_occurrences(answer['text'], len(pack['evidence'])))
        expected = {f'S{i+1}': evidence['reference'] for i, evidence in enumerate(pack['evidence'])}
        citations = answer['citations']
        if (len(citations) != len(labels) or {c['label'] for c in citations} != labels
                or any(c['reference'] != expected[c['label']] for c in citations)):
            raise ValueError('Citation labels/references differ from supplied evidence')
        if answer['referenceValidation'] != ('valid' if labels else 'not_applicable'):
            raise ValueError('Incorrect mechanical reference validation')
        if answer['semanticValidation'] != 'not_evaluated':
            raise ValueError('Capture requires separate semantic review')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('binary', 'source', 'manifest', 'suite', 'store', 'output'):
        parser.add_argument('--'+name, type=Path, required=True)
    parser.add_argument('--profile', required=True)
    parser.add_argument('--flash-bundle', type=Path)
    parser.add_argument('--generation-profile', type=Path)
    parser.add_argument('--embedding-bundle', type=Path)
    parser.add_argument('--generation-startup-deadline-ms', type=int)
    parser.add_argument('--monitor-system-pressure', action='store_true')
    parser.add_argument('--mode', choices=['lexical', 'hybrid'], required=True)
    parser.add_argument('--split', choices=['tuning', 'held_out'], default='held_out')
    parser.add_argument('--allow-draft', action='store_true')
    parser.add_argument('--allow-degraded', action='store_true')
    parser.add_argument('--evidence-budget', type=int, default=3000)
    parser.add_argument('--input-budget', type=int, default=4096)
    parser.add_argument('--output-budget', type=int, default=1024)
    parser.add_argument('--deadline-ms', type=int, default=30000)
    args = parser.parse_args()
    root, store, output = args.source.resolve(), args.store.resolve(), args.output.resolve()
    for destination in (store, output):
        if destination == root or root in destination.parents or destination in root.parents:
            parser.error('Evaluation destinations must be outside the read-only source tree')
        if destination.exists():
            parser.error('Store and capture destinations must be new')
    if store == output or store in output.parents or output in store.parents:
        parser.error('Store and capture destinations must be separate')
    if bool(args.flash_bundle) != bool(args.generation_profile):
        parser.error('Explicit Flash/profile bundles must be selected together')
    if (not 1 <= args.evidence_budget <= 32768 or not 1 <= args.input_budget <= 32768
            or not 1 <= args.output_budget <= 4096 or not 1 <= args.deadline_ms <= 120000):
        parser.error('Invalid answer budgets/deadline')
    if args.generation_startup_deadline_ms is not None and not 1000 <= args.generation_startup_deadline_ms <= 900000:
        parser.error('Generation startup deadline must be 1000–900000 ms')
    pressure_receipt=output.with_suffix('.pressure.json')
    if args.monitor_system_pressure:
        if pressure_receipt.exists():parser.error('Pressure receipt must be new')
        if memory_pressure_level()!=1:parser.error('Defer GPU evaluation until memory pressure is normal')
    tracked = {'binary': args.binary.resolve(), 'suite': args.suite.resolve(), 'manifest': args.manifest.resolve(),
               'captureScript': Path(__file__).resolve(), 'retrievalScript': Path(__file__).with_name('evaluate_retrieval.py'),
               'clientScript': Path(__file__).with_name('local_client.py'),
               'citationScript': Path(__file__).with_name('citations.py')}
    options = []
    for name in ('flash_bundle', 'generation_profile', 'embedding_bundle'):
        selected = getattr(args, name)
        if selected:
            selected = selected.resolve()
            tracked[name] = selected/'bundle.json'
            options += ['--'+name.replace('_', '-'), str(selected)]
    if args.generation_startup_deadline_ms is not None:
        options += ['--generation-startup-deadline-ms',str(args.generation_startup_deadline_ms)]
    if args.monitor_system_pressure:
        tracked['pressureMonitorScript']=Path(__file__).with_name('pressure_monitor.py')
    identities = {name: digest(path.read_bytes()) for name, path in tracked.items()}
    suite, manifest = json.loads(tracked['suite'].read_bytes()), json.loads(tracked['manifest'].read_bytes())
    identity, files = snapshot(root)
    validate_suite(suite, manifest, files, identity)
    if suite['annotationStatus'] not in {'human_reviewed', 'agent_reviewed'} and not args.allow_draft:
        parser.error('Draft/fixture suites require --allow-draft')
    questions = [q for q in suite['questions'] if q['split'] == args.split]
    if not questions:
        parser.error('Selected split has no questions')
    settings = dict(profile=args.profile, mode=args.mode, strictHybrid=not args.allow_degraded,
        evidenceTokenBudget=args.evidence_budget, inputTokenBudget=args.input_budget,
        outputTokenBudget=args.output_budget, deadlineMs=args.deadline_ms,
        queueDeadlineMs=args.deadline_ms, queuePolicy='reject')
    rows = []
    startup_started=time.monotonic()
    with LocalClient(args.binary.resolve(), store, options) as client, (protect_owned_work(client,pressure_receipt) if args.monitor_system_pressure else nullcontext()):
        startup_status=client.call('status',timeout=(args.generation_startup_deadline_ms or 120000)/1000+60)
        startup_ms=(time.monotonic()-startup_started)*1000
        print(json.dumps(dict(stage='model_startup_finished',startupMs=startup_ms,status=startup_status['status'])),flush=True)
        for operation, fields in [('register', dict(name='kb', root=str(root))), ('reconcile', dict(collection='kb'))]:
            result = client.call(operation, **fields)
            if result['status'] != 'ok' or (operation == 'reconcile' and result['data']['failures']):
                raise ValueError('Import failed; no answer evaluation published')
        unavailable = False
        for question in questions:
            trace = AnswerTrace(files, settings)
            transport_error = None
            if not unavailable:
                try:
                    client.call('answer', question=question['query'], collections=['kb'], **settings,
                                timeout=args.deadline_ms/1000+2, progress=trace.observe)
                except (RuntimeError, TimeoutError, BrokenPipeError):
                    unavailable = True
                    transport_error = 'coordinator_unavailable'
            else:
                transport_error = 'coordinator_unavailable'
            if trace.terminal_ms is None and not transport_error:
                raise ValueError('Answer stream ended without a terminal outcome')
            if trace.answer is not None:
                # Resolve every cited immutable span through the actual interface.
                for citation in trace.answer['citations']:
                    context = client.call('context', reference=citation['reference'])
                    reference = citation['reference']
                    evidence = next(e for e in trace.pack['evidence'] if e['reference'] == reference)
                    if context['status'] != 'ok' or context['data']['text'] != evidence['text']:
                        raise ValueError('Completed citation does not resolve exactly')
            rows.append(dict(id=question['id'], classes=question['classes'], answerable=question['answerable'],
                query=question['query'], requiredClaims=question['claims'], events=trace.events,
                requiredEvidence=[dict(span, text=files[span['path']][span['byteStart']:span['byteEnd']].decode())
                    for claim in question['claims'] for span in claim['support']],
                outcome='completed' if trace.answer is not None else 'failed',
                errorCode=transport_error or trace.error, answer=trace.answer,
                firstVisibleClientMs=trace.first_visible_ms, terminalClientMs=trace.terminal_ms))
            print(json.dumps(dict(stage='answer_finished', id=question['id'], outcome=rows[-1]['outcome'],
                                  errorCode=rows[-1]['errorCode'])), flush=True)
    if snapshot(root)[0] != identity or any(digest(path.read_bytes()) != identities[name] for name, path in tracked.items()):
        raise ValueError('Evaluation inputs changed; no capture published')
    report = dict(schemaVersion=1, kind='local-answer-capture', corpusManifestSha256=identity,
        inputSha256=identities, split=args.split, annotationStatus=suite['annotationStatus'],
        annotationReviewer=suite.get('reviewer'), settings=settings, questions=rows,
        questionCount=len(rows), failures=sum(r['outcome'] != 'completed' for r in rows), sourceUnchanged=True,
        runtime=dict(python=platform.python_version(), platform=platform.platform()), releaseQualified=False,
        execution=dict(order='suite order, sequential requests', samplesPerQuestion=1,startupMs=startup_ms,
            generationStartupDeadlineMs=args.generation_startup_deadline_ms or 120000,
            startupStatus=startup_status,pressureMonitorReceiptSha256=digest(pressure_receipt.read_bytes()) if args.monitor_system_pressure else None,
            coordinator='new resident process; startup/import excluded from request timing',
            caches='first request and later requests retained in order; no result-cache feature enabled'),
        limitations=['Private trace: contains questions, claims, source evidence, and answers',
            'Mechanical reference/token checks do not establish factual correctness or entailment',
            'Separate declared source review required; annotation/reviewer status is declared provenance',
            'Client timings include IPC/validation callbacks, exclude subsequent context verification',
            'No independent OS network denial, hardware-tier, resource, or competitive qualification',
            'Selected bundle manifests are hashed; no claim that assets stayed immutable between checks'])
    write_new(output, report)
    print(json.dumps(dict(questionCount=len(rows), failures=report['failures'], releaseQualified=False)))
    if report['failures']:
        raise SystemExit(2)


if __name__ == '__main__':
    main()
