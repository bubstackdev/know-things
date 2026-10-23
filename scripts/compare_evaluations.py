#!/usr/bin/env python3
"""Paired supporting-span comparisons; never certifies annotation or release quality."""
import argparse
import hashlib
import json
import math
import random
from pathlib import Path


def paired_values(baseline, candidate, metric):
    for key in ['schemaVersion', 'kind', 'corpusManifestSha256', 'suiteSha256', 'split',
                'annotationStatus', 'evidenceBudget', 'budgetTokenizer', 'deadlineMs']:
        if baseline.get(key) != candidate.get(key) or baseline.get(key) is None:
            raise ValueError(f'Incompatible comparison field: {key}')
    if baseline['kind'] != 'supporting-span-retrieval-evaluation' or baseline['schemaVersion'] != 1:
        raise ValueError('Unsupported evaluation semantics')
    if not baseline.get('sourceUnchanged') or not candidate.get('sourceUnchanged'):
        raise ValueError('Both source snapshots must be verified unchanged')
    def indexed(report):
        rows = {row['id']: row for row in report['questions']}
        if len(rows) != len(report['questions']):
            raise ValueError('Duplicate question IDs')
        return rows
    old, new = indexed(baseline), indexed(candidate)
    if old.keys() != new.keys() or not old:
        raise ValueError('Paired comparisons require identical nonempty question sets')
    pairs = []
    for identifier in sorted(old):
        a, b = old[identifier], new[identifier]
        if a['answerable'] != b['answerable'] or a['classes'] != b['classes']:
            raise ValueError('Question labels differ')
        if not a['answerable']:
            if a['scores'] is not None or b['scores'] is not None:
                raise ValueError('Unanswerable questions cannot receive recall credit')
            continue
        values = [row['scores'].get(metric) for row in [a, b]]
        if any(type(v) not in [int, float] or not math.isfinite(v) or not 0 <= v <= 1 for v in values):
            raise ValueError('Metric must have finite unit-interval scores for every answerable question')
        if any(row.get('errorCode') and value != 0 for row, value in zip([a, b], values)):
            raise ValueError('Failed queries must remain zero-scored pairs')
        pairs.append(dict(id=identifier, classes=a['classes'], baseline=values[0], candidate=values[1]))
    if not pairs:
        raise ValueError('No answerable pairs')
    return pairs


def interval(pairs, samples, seed):
    differences = [row['candidate']-row['baseline'] for row in pairs]
    n = len(differences)
    randomizer = random.Random(seed)
    means = sorted(sum(differences[randomizer.randrange(n)] for _ in range(n))/n for _ in range(samples))
    return dict(pairs=n, baselineMean=sum(row['baseline'] for row in pairs)/n,
                candidateMean=sum(row['candidate'] for row in pairs)/n,
                meanDifference=sum(differences)/n,
                percentile95=[means[int((samples-1)*.025)], means[int((samples-1)*.975)]])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--metric', default='claimRecallAt20')
    parser.add_argument('--samples', type=int, default=10000)
    parser.add_argument('--seed', type=int, default=20261022)
    args = parser.parse_args()
    if not 1000 <= args.samples <= 100000:
        parser.error('Use 1000–100000 bootstrap samples')
    if args.output.exists():
        parser.error('Comparison destination must be new')
    a_bytes, b_bytes = args.baseline.read_bytes(), args.candidate.read_bytes()
    baseline, candidate = json.loads(a_bytes), json.loads(b_bytes)
    pairs = paired_values(baseline, candidate, args.metric)
    report = dict(schemaVersion=1, kind='paired-supporting-span-comparison', metric=args.metric,
        baselineSha256=hashlib.sha256(a_bytes).hexdigest(), candidateSha256=hashlib.sha256(b_bytes).hexdigest(),
        corpusManifestSha256=baseline['corpusManifestSha256'], suiteSha256=baseline['suiteSha256'],
        split=baseline['split'], annotationStatus=baseline['annotationStatus'],
        bootstrapSamples=args.samples, seed=args.seed, overall=interval(pairs, args.samples, args.seed),
        byClass={label: interval([p for p in pairs if label in p['classes']], args.samples, args.seed)
                 for label in sorted({label for p in pairs for label in p['classes']})},
        excludedUnanswerable=len(baseline['questions'])-len(pairs), releaseQualified=False,
        limitations=['Paired percentile bootstrap resamples questions, not correlated paraphrase groups',
                     'Small slices and shared documents limit generalization',
                     'Confidence intervals do not certify labels, entailment, or held-out quality',
                     'No latency, resource, answer-quality, or competitive win claim'])
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open('x') as stream:
        json.dump(report, stream, indent=2)
        stream.write('\n')
    print(json.dumps(report['overall']))


if __name__ == '__main__':
    main()
