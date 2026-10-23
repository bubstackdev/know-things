#!/usr/bin/env python3
"""Read-only corpus smoke/latency run. Private evidence never appears in the summary."""
import argparse
import hashlib
import json
import platform
import subprocess
import time
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', default='target/debug/know-things')
    parser.add_argument('--source', required=True)
    parser.add_argument('--store', required=True)
    parser.add_argument('--output', required=True)
    parser.add_argument('--samples', type=int, default=100)
    parser.add_argument('--manifest', type=Path)
    parser.add_argument('--build-kind', choices=['debug', 'release'], default='debug')
    args = parser.parse_args()
    root = Path(args.source).resolve()
    before = {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
              for p in sorted(root.rglob('*.md')) if '.git' not in p.parts}
    if args.samples < 1:
        parser.error('samples must be positive')
    canonical_manifest = hashlib.sha256(''.join(f'{path}\t{digest}\n' for path, digest in sorted(before.items())).encode()).hexdigest()
    if args.manifest:
        captured = json.loads(args.manifest.read_text())
        assert canonical_manifest == captured['contentManifestSha256'], 'Corpus differs from the declared snapshot'
    binary_sha256 = hashlib.sha256(Path(args.binary).read_bytes()).hexdigest()
    git_env = dict(__import__('os').environ, GIT_OPTIONAL_LOCKS='0')
    git = lambda *arguments: subprocess.run(['git', '-C', str(root), *arguments], env=git_env, capture_output=True, text=True)
    commit = git('rev-parse', 'HEAD')
    git_status = git('status', '--porcelain')
    process = subprocess.Popen([args.binary, 'local', args.store], stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, text=True, bufsize=1)
    serial = 0
    def call(operation, **fields):
        nonlocal serial
        serial += 1
        process.stdin.write(json.dumps(dict(schemaVersion=1, requestId=str(serial), operation=operation, **fields))+'\n')
        process.stdin.flush()
        line = process.stdout.readline()
        if not line:
            raise RuntimeError('Coordinator exited')
        result = json.loads(line)
        if result['status'] != 'ok':
            raise RuntimeError(result.get('error'))
        return result
    try:
        call('register', name='kb', root=str(root))
        started = time.perf_counter()
        ingestion = call('reconcile', collection='kb')
        ingestion_ms = (time.perf_counter()-started)*1000
        queries = ['scheduled job run result', 'MariaDB Elasticsearch integration tests',
                   'distributed cache policies', 'GitLab CI template references',
                   'documentation retrieval context', 'shared integration credential',
                   'what changed in production today']
        durations, counts, tokens = [], [], []
        for i in range(args.samples+len(queries)):
            result = call('search', query=queries[i % len(queries)], collections=['kb'],
                          mode='lexical', topK=20, evidenceTokenBudget=12000)
            pack = result['data']
            # Resolve every returned reference against exact original bytes.
            for evidence in pack['evidence']:
                context = call('context', reference=evidence['reference'])
                assert context['data']['text'] == evidence['text']
            if i >= len(queries):
                durations.append(result['durationMs'])
                counts.append(len(pack['evidence']))
                tokens.append(pack['tokenCount'])
        integrity = call('integrity')['data']
        status = call('status')['data']
    finally:
        process.stdin.close()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.terminate()  # Only this script's child.
            process.wait(timeout=5)
    after = {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
             for p in sorted(root.rglob('*.md')) if '.git' not in p.parts}
    assert before == after, 'Source snapshot changed during measurement'
    durations.sort()
    percentile = lambda p: durations[min(len(durations)-1, int((len(durations)-1)*p))]
    summary = dict(kind='lexical-smoke-not-quality-qualification', buildKind=args.build_kind, samples=args.samples,
                   queryClasses=len(queries), hardware=platform.machine(), platform=platform.platform(),
                   sourceFiles=len(before), sourceManifestHash=canonical_manifest, binarySha256=binary_sha256,
                   gitCommit=commit.stdout.strip() if commit.returncode == 0 else None,
                   gitWorkingTreeClean=not git_status.stdout.strip() if git_status.returncode == 0 else None,
                   ingestion=ingestion['data'], ingestionMs=ingestion_ms,
                   budgetTokenizer='utf8-byte-v1', evidenceBudget=12000,
                   latencyMs=dict(p50=percentile(.5), p95=percentile(.95), p99=percentile(.99)),
                   meanEvidenceCount=sum(counts)/len(counts), maxBudgetTokens=max(tokens),
                   integrity=integrity, sourceUnchanged=True, status=status,
                   limitations=[args.build_kind.capitalize()+' build', 'Unlabeled diagnostic queries; no recall/answer-quality claim',
                                'No semantic model or generation', '100 samples provide only a preliminary tail estimate',
                                'Contention not isolated; no other processes were controlled'])
    # Exclude private source paths from publishable summary.
    for row in summary['status']['collections']:
        row.pop('root', None)
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(summary, indent=2)+'\n')
    print(json.dumps({k: summary[k] for k in ['samples', 'sourceFiles', 'latencyMs', 'integrity', 'sourceUnchanged']}))


if __name__ == '__main__':
    main()
