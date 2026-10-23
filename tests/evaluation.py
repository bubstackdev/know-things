#!/usr/bin/env python3
"""Evaluation correctness and installed-coordinator checks; no model assets needed."""
import argparse
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT/'scripts'))
import evaluate_retrieval as evaluation
import compare_evaluations as comparison


def span(raw, start, end, path='guide.md'):
    return dict(path=path, contentHash=evaluation.digest(raw), byteStart=start, byteEnd=end)


def evidence(raw, start, end, path='guide.md'):
    return dict(source=path, reference=dict(contentHash=evaluation.digest(raw), byteStart=start, byteEnd=end))


class Evaluation(unittest.TestCase):
    def test_paired_comparisons_validate_units_pair_by_id_and_preserve_failures(self):
        common = dict(schemaVersion=1, kind='supporting-span-retrieval-evaluation', corpusManifestSha256='corpus',
            suiteSha256='suite', split='tuning', annotationStatus='fixture', evidenceBudget=100,
            budgetTokenizer='utf8-byte-v1', deadlineMs=5000, sourceUnchanged=True)
        def row(identifier, value, error=None):
            return dict(id=identifier, classes=['fixture'], answerable=True,
                        scores={'claimRecallAt20': value}, errorCode=error)
        baseline = dict(common, questions=[row('one', 0, 'deadline_exceeded'), row('two', 1)])
        candidate = dict(common, questions=[row('two', 1), row('one', 1)])
        pairs = comparison.paired_values(baseline, candidate, 'claimRecallAt20')
        self.assertEqual([p['id'] for p in pairs], ['one', 'two'])
        result = comparison.interval(pairs, 1000, 42)
        self.assertEqual(result['meanDifference'], .5)
        self.assertEqual(result['percentile95'], [0., 1.])
        with self.assertRaises(ValueError):
            comparison.paired_values(baseline, dict(candidate, evidenceBudget=101), 'claimRecallAt20')
        candidate['questions'][0]['scores']['claimRecallAt20'] = float('nan')
        with self.assertRaises(ValueError):
            comparison.paired_values(baseline, candidate, 'claimRecallAt20')

    def test_adjacent_spans_cover_support_but_gaps_and_wrong_revisions_do_not(self):
        raw = b'0123456789'
        gold = span(raw, 2, 8)
        self.assertTrue(evaluation.covers(gold, [evidence(raw, 1, 5), evidence(raw, 5, 9)]))
        self.assertFalse(evaluation.covers(gold, [evidence(raw, 1, 4), evidence(raw, 5, 9)]))
        self.assertFalse(evaluation.covers(gold, [evidence(b'other revision', 0, 10)]))
        self.assertFalse(evaluation.covers(gold, [evidence(raw, 0, 10, 'another.md')]))

    def test_unanswerable_rows_do_not_inflate_recall_and_failed_queries_remain_zero(self):
        raw = b'policy'
        question = dict(answerable=True, claims=[dict(support=[span(raw, 0, 6)])])
        good = evaluation.score(question, [evidence(raw, 0, 6)])
        failed = evaluation.score(question, [])
        unknown = evaluation.score(dict(answerable=False), [])
        self.assertIsNone(unknown)
        result = evaluation.aggregate([dict(scores=s) for s in [good, failed, unknown]])
        self.assertEqual(result['claimRecallAt20'], .5)
        self.assertEqual(result['completeEvidenceAt20'], .5)

    def test_support_requires_exact_hashes_and_utf8_boundaries(self):
        raw = 'Café policy'.encode()
        identity = evaluation.digest(f'guide.md\t{evaluation.digest(raw)}\n'.encode())
        manifest = dict(schemaVersion=1, contentManifestSha256=identity,
                        files=[dict(path='guide.md', sha256=evaluation.digest(raw), bytes=len(raw))])
        support = span(raw, 0, 5)
        suite = dict(schemaVersion=1, annotationStatus='fixture', corpusManifestSha256=identity,
            questions=[dict(id='q1', split='held_out', query='policy', classes=['identifier'], answerable=True,
                            claims=[dict(text='A claim', support=[support])])])
        evaluation.validate_suite(suite, manifest, {'guide.md': raw}, identity)
        suite.update(annotationStatus='agent_reviewed', reviewer='Agent fixture source auditor', sourceAuditSha256='a'*64)
        evaluation.validate_suite(suite, manifest, {'guide.md': raw}, identity)
        for value in [None, 'not-a-digest']:
            suite['sourceAuditSha256'] = value
            with self.assertRaises(ValueError):
                evaluation.validate_suite(suite, manifest, {'guide.md': raw}, identity)
        suite['sourceAuditSha256'] = 'a'*64
        suite['reviewer'] = ''
        with self.assertRaises(ValueError):
            evaluation.validate_suite(suite, manifest, {'guide.md': raw}, identity)
        suite['reviewer'] = 'Agent fixture source auditor'
        support['byteStart'] = 4  # Continuation byte of é.
        with self.assertRaises(UnicodeDecodeError):
            evaluation.validate_suite(suite, manifest, {'guide.md': raw}, identity)
        support['byteStart'] = 0
        support['contentHash'] = 'wrong'
        with self.assertRaises(ValueError):
            evaluation.validate_suite(suite, manifest, {'guide.md': raw}, identity)

    def test_installed_retrieval_and_strict_hybrid_failures_produce_honest_scores(self):
        with tempfile.TemporaryDirectory(prefix='know-things-eval-') as directory:
            root = Path(directory)
            source = root/'source'
            source.mkdir()
            documents = {'alpha.md': b'Alpha requires a local cache.\n',
                         'beta.md': b'Beta requires a pinned backup.\n'}
            for name, raw in documents.items():
                (source/name).write_bytes(raw)
            identity, files = evaluation.snapshot(source)
            manifest = dict(schemaVersion=1, contentManifestSha256=identity,
                files=[dict(path=name, sha256=evaluation.digest(raw), bytes=len(raw)) for name, raw in files.items()])
            claims = [dict(text=raw.decode().strip(), support=[span(raw, 0, len(raw), name)]) for name, raw in documents.items()]
            questions = [dict(id='q1', split='held_out', query='Alpha', classes=['identifier'], answerable=True, claims=claims[:1]),
                         dict(id='q2', split='held_out', query='Alpha Beta', classes=['multi_source'], answerable=True, claims=claims),
                         dict(id='q3', split='held_out', query='current weather', classes=['unanswerable'], answerable=False, claims=[])]
            suite = dict(schemaVersion=1, annotationStatus='fixture', corpusManifestSha256=identity, questions=questions)
            (root/'manifest.json').write_text(json.dumps(manifest))
            (root/'suite.json').write_text(json.dumps(suite))
            base = [sys.executable, str(ROOT/'scripts/evaluate_retrieval.py'), '--binary', str(BINARY),
                    '--source', str(source), '--manifest', str(root/'manifest.json'), '--suite', str(root/'suite.json'), '--allow-draft']
            for mode in ['lexical', 'hybrid']:
                output = root/f'{mode}.json'
                result = subprocess.run(base+['--mode', mode, '--store', str(root/mode), '--output', str(output)],
                                        capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode, 0 if mode == 'lexical' else 2, result.stderr)
                report = json.loads(output.read_text())
                self.assertEqual(report['metrics']['claimRecallAt20'], 1. if mode == 'lexical' else 0.)
                self.assertEqual(report['failures'], 0 if mode == 'lexical' else 3)
                self.assertIsNone(report['questions'][2]['scores'])
                self.assertIsNone(report['byClass']['unanswerable'])
                self.assertFalse(report['releaseQualified'])
                self.assertNotIn('Alpha requires', output.read_text())
                if mode == 'hybrid':
                    self.assertTrue(all(row['actualMode'] is None for row in report['questions']))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    args, remaining = parser.parse_known_args()
    BINARY = args.binary.resolve()
    unittest.main(argv=[sys.argv[0], *remaining])
