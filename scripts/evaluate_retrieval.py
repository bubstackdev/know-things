#!/usr/bin/env python3
"""Evaluate revision-pinned supporting spans. No inference judge or network calls."""
import argparse
import hashlib
import json
import platform
import re
import time
from pathlib import Path
from local_client import LocalClient


def digest(data):
    return hashlib.sha256(data).hexdigest()


def snapshot(root):
    root = root.resolve()
    excluded = {'.git', 'node_modules', 'target', 'dist', 'build', '.private'}
    files = {}
    for path in sorted(root.rglob('*')):
        relative = path.relative_to(root)
        if any(part in excluded for part in relative.parts):
            continue
        if path.suffix.lower() not in {'.md', '.markdown', '.txt'}:
            continue
        if path.is_symlink() or not path.is_file() or path.resolve() != path:
            raise ValueError('Evaluation sources must be regular files without symlinks')
        raw = path.read_bytes()
        raw.decode('utf-8')
        files[relative.as_posix()] = raw
    identity = digest(''.join(f'{name}\t{digest(raw)}\n' for name, raw in sorted(files.items())).encode())
    return identity, files


def validate_suite(suite, manifest, files, identity):
    if suite.get('schemaVersion') != 1 or suite.get('corpusManifestSha256') != identity:
        raise ValueError('Suite version or corpus snapshot mismatch')
    if manifest.get('schemaVersion') != 1 or manifest['contentManifestSha256'] != identity:
        raise ValueError('Manifest version or corpus snapshot mismatch')
    expected = {row['path']: (row['sha256'], row['bytes']) for row in manifest['files']}
    if expected != {name: (digest(raw), len(raw)) for name, raw in files.items()}:
        raise ValueError('Source files differ from the corpus manifest')
    status = suite.get('annotationStatus')
    if status not in {'agent_draft', 'agent_reviewed', 'human_reviewed', 'fixture'}:
        raise ValueError('Declare annotationStatus explicitly')
    if status in {'agent_reviewed', 'human_reviewed'} and (
            not isinstance(suite.get('reviewer'), str) or not suite['reviewer'].strip()):
        raise ValueError('Reviewed suites require reviewer provenance')
    if status == 'agent_reviewed' and (not isinstance(suite.get('sourceAuditSha256'), str)
            or not re.fullmatch(r'[0-9a-f]{64}', suite['sourceAuditSha256'])):
        raise ValueError('Agent-reviewed suites require a source-audit digest')
    seen = set()
    questions = suite.get('questions', [])
    if not questions:
        raise ValueError('Suite has no questions')
    for question in questions:
        identifier = question.get('id', '')
        if not re.fullmatch(r'[a-zA-Z0-9_-]{1,64}', identifier) or identifier in seen:
            raise ValueError('Question IDs must be unique bounded slugs')
        seen.add(identifier)
        if question.get('split') not in {'tuning', 'held_out'}:
            raise ValueError('Every question needs an explicit split')
        if (not isinstance(question.get('query'), str) or not question['query'].strip()
                or len(question['query'].encode()) > 8192):
            raise ValueError('Invalid query length')
        if (not isinstance(question.get('classes'), list) or not question['classes']
                or not all(isinstance(c, str) and re.fullmatch(r'[a-zA-Z0-9_-]{1,64}', c) for c in question['classes'])):
            raise ValueError('Question classes are required')
        if type(question.get('answerable')) is not bool:
            raise ValueError('Answerability must be explicit')
        claims = question.get('claims', [])
        if not isinstance(claims, list) or bool(claims) != question['answerable']:
            raise ValueError('Answerable questions need claims; unanswerable questions must have none')
        for claim in claims:
            if (not isinstance(claim.get('text'), str) or not claim['text'].strip()
                    or not isinstance(claim.get('support'), list) or not claim['support']):
                raise ValueError('Each claim needs text and supporting spans')
            for span in claim['support']:
                raw = files.get(span.get('path'))
                start, end = span.get('byteStart'), span.get('byteEnd')
                if raw is None or span.get('contentHash') != digest(raw):
                    raise ValueError('Supporting source identity mismatch')
                if type(start) is not int or type(end) is not int or not 0 <= start < end <= len(raw):
                    raise ValueError('Invalid supporting span')
                raw[start:end].decode('utf-8')


def covers(span, evidence):
    intervals = sorted((e['reference']['byteStart'], e['reference']['byteEnd'])
                       for e in evidence if e['source'] == span['path']
                       and e['reference']['contentHash'] == span['contentHash'])
    covered = span['byteStart']
    for start, end in intervals:
        if start > covered:
            break
        covered = max(covered, end)
        if covered >= span['byteEnd']:
            return True
    return False


def score(question, evidence):
    if not question['answerable']:
        return None
    claims = question['claims']
    spans = [span for claim in claims for span in claim['support']]
    result = {}
    for k in (5, 10, 20):
        selected = evidence[:k]
        result[f'claimRecallAt{k}'] = sum(all(covers(s, selected) for s in claim['support'])
                                         for claim in claims)/len(claims)
        result[f'supportSpanRecallAt{k}'] = sum(covers(s, selected) for s in spans)/len(spans)
    result['completeEvidenceAt20'] = float(result['claimRecallAt20'] == 1)
    first = next((k for k in range(1, min(20, len(evidence))+1)
                  if any(covers(span, evidence[:k]) for span in spans)), None)
    result['firstSupportReciprocalRankAt20'] = 1/first if first else 0.
    return result


def verify_pack(pack, files, budget):
    if pack['budgetTokenizer'] != 'utf8-byte-v1' or pack['tokenCount'] > budget:
        raise ValueError('Unexpected budget tokenizer or budget overflow')
    counted = 0
    for evidence in pack['evidence']:
        reference = evidence['reference']
        raw = files[evidence['source']]
        start, end = reference['byteStart'], reference['byteEnd']
        if (reference['contentHash'] != digest(raw) or not 0 <= start < end <= len(raw)
                or raw[start:end] != evidence['text'].encode('utf-8')):
            raise ValueError('Returned evidence is not byte-exact in the declared snapshot')
        if evidence['tokenCount'] != end-start:
            raise ValueError('Incorrect byte-budget count')
        counted += evidence['tokenCount']
    if counted != pack['tokenCount']:
        raise ValueError('Evidence-pack budget count mismatch')


def aggregate(rows):
    scored = [row['scores'] for row in rows if row['scores'] is not None]
    return {key: sum(row[key] for row in scored)/len(scored) for key in scored[0]} if scored else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--suite', type=Path, required=True)
    parser.add_argument('--store', type=Path, required=True, help='New disposable store outside the source tree')
    parser.add_argument('--output', type=Path, required=True, help='New summary file; omits queries/source text')
    parser.add_argument('--mode', choices=['lexical', 'hybrid'], required=True)
    parser.add_argument('--embedding-bundle', type=Path)
    parser.add_argument('--split', choices=['tuning', 'held_out'], default='held_out')
    parser.add_argument('--allow-draft', action='store_true')
    parser.add_argument('--allow-degraded', action='store_true')
    parser.add_argument('--budget', type=int, default=12000)
    parser.add_argument('--deadline-ms', type=int, default=5000)
    args = parser.parse_args()
    root, store, output = args.source.resolve(), args.store.resolve(), args.output.resolve()
    if store == root or root in store.parents or output == root or root in output.parents:
        parser.error('Evaluation writes must be outside the read-only source tree')
    if store.exists() or output.exists():
        parser.error('Store and summary must be new to preserve run provenance')
    if not 1 <= args.budget <= 65536 or not 1 <= args.deadline_ms <= 120000:
        parser.error('Invalid evidence budget or query deadline')
    suite_bytes = args.suite.read_bytes()
    suite, manifest = json.loads(suite_bytes), json.loads(args.manifest.read_bytes())
    identity, files = snapshot(root)
    validate_suite(suite, manifest, files, identity)
    if suite['annotationStatus'] not in {'human_reviewed', 'agent_reviewed'} and not args.allow_draft:
        parser.error('Draft/fixture annotations require --allow-draft and do not qualify quality')
    questions = [q for q in suite['questions'] if q['split'] == args.split]
    if not questions:
        parser.error('Selected split has no questions')
    options = ['--embedding-bundle', args.embedding_bundle] if args.embedding_bundle else []
    binary_sha256 = digest(args.binary.read_bytes())
    rows = []
    with LocalClient(args.binary.resolve(), store, options) as client:
        for operation, fields in [('register', dict(name='kb', root=str(root))), ('reconcile', dict(collection='kb'))]:
            response = client.call(operation, **fields)
            if response['status'] != 'ok' or (operation == 'reconcile' and response['data']['failures']):
                raise RuntimeError('Evaluation import failed; no quality result published')
        unavailable = False
        for question in questions:
            started = time.monotonic()
            try:
                if unavailable:
                    raise RuntimeError('Coordinator unavailable after a prior transport failure')
                response = client.call('search', query=question['query'], collections=['kb'], topK=20,
                    evidenceTokenBudget=args.budget, deadlineMs=args.deadline_ms, mode=args.mode,
                    strictHybrid=not args.allow_degraded, timeout=args.deadline_ms/1000+1)
                error = response.get('error')
                pack = response.get('data') if not error else None
                if pack:
                    verify_pack(pack, files, args.budget)
            except (RuntimeError, TimeoutError, BrokenPipeError):
                unavailable = True
                error, pack = dict(code='coordinator_unavailable'), None
            rows.append(dict(id=question['id'], classes=question['classes'], answerable=question['answerable'],
                errorCode=error['code'] if error else None,
                elapsedMs=(time.monotonic()-started)*1000,
                actualMode=pack['actualMode'] if pack else None,
                degradationReasons=pack['degradationReasons'] if pack else [],
                embeddingSpaceId=pack['embeddingSpaceId'] if pack else None,
                evidencePackId=pack['evidencePackId'] if pack else None,
                evidenceCount=len(pack['evidence']) if pack else 0,
                scores=score(question, pack['evidence'] if pack else [])))
    after, _ = snapshot(root)
    if after != identity:
        raise ValueError('Source changed during evaluation; no quality result published')
    if digest(args.binary.read_bytes()) != binary_sha256:
        raise ValueError('Binary changed during evaluation; no quality result published')
    summary = dict(schemaVersion=1, kind='supporting-span-retrieval-evaluation',
        corpusManifestSha256=identity, suiteSha256=digest(suite_bytes), split=args.split,
        annotationStatus=suite['annotationStatus'], binarySha256=binary_sha256,
        runtime=dict(python=platform.python_version(), platform=platform.platform()),
        requestedMode=args.mode, strictHybrid=not args.allow_degraded,
        evidenceBudget=args.budget, budgetTokenizer='utf8-byte-v1', deadlineMs=args.deadline_ms,
        questionCount=len(rows), failures=sum(row['errorCode'] is not None for row in rows),
        answerableCount=sum(row['answerable'] for row in rows),
        metrics=aggregate(rows), byClass={label: aggregate([r for r in rows if label in r['classes']])
                                        for label in sorted({label for r in rows for label in r['classes']})},
        questions=rows, sourceUnchanged=True, releaseQualified=False,
        limitations=['Annotation status is declared provenance, not independently certified',
            'Supporting-span coverage is mechanical; claim entailment needs reviewed labels',
            'Unanswerable questions are excluded from recall, not scored as automatic successes',
            'No answer generation, abstention quality, nDCG, or competitive qualification',
            'Timeout/error rows remain in answerable recall denominators',
            'No result-cache hits or isolated-contention performance claim'])
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open('x') as stream:
        json.dump(summary, stream, indent=2)
        stream.write('\n')
    print(json.dumps({key: summary[key] for key in ['questionCount', 'failures', 'annotationStatus', 'metrics', 'sourceUnchanged']}))
    if summary['failures']:
        raise SystemExit(2)


if __name__ == '__main__':
    main()
