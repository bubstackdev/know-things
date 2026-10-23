#!/usr/bin/env python3
"""Answer capture/review invariants; every judgment here is a synthetic fixture."""
import copy
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT/'scripts'))
import evaluate_answers as capture
import score_answers as scoring


def stream():
    raw = b'Redis is required.\n'
    reference = dict(contentHash=capture.digest(raw), byteStart=0, byteEnd=len(raw))
    pack = dict(tokenCount=30, budgetTokenizer='fixture-tokenizer', actualMode='lexical',
        embeddingSpaceId=None, degradationReasons=[], evidencePackId='fixture-pack', corpusGeneration=1,
        evidence=[dict(source='guide.md', reference=reference, text=raw.decode())])
    answer = dict(text='Use Redis [S1].', evidenceTokens=30, evidencePackId='fixture-pack', corpusGeneration=1,
        actualMode='lexical', embeddingSpaceId=None, degradationReasons=[],
        profile=dict(tokenizerId='fixture-tokenizer', contextLimit=4096),
        promptTokens=100, generatedTokens=8, visibleTokens=6, hiddenTokens=2, finishReason='stop', visibleTokenCountKind='generated',
        referenceValidation='valid', semanticValidation='not_evaluated', timingsMs={'total': 10.},
        citations=[dict(label='S1', reference=reference)])
    settings = dict(evidenceTokenBudget=100, inputTokenBudget=200, outputTokenBudget=20,
                    mode='lexical', strictHybrid=True)
    events = [dict(event='accepted'), dict(event='evidence_ready', pack=pack),
              dict(event='answer_delta', text=answer['text']), dict(event='completed', answer=answer)]
    envelopes = [dict(status='ok' if i == 4 else 'streaming',
                     data=dict(runId='fixture-run', sequence=i, **event)) for i, event in enumerate(events, 1)]
    return {'guide.md': raw}, settings, envelopes


def records():
    answer = stream()[2][-1]['data']['answer']
    def row(identifier, answerable=True, outcome='completed', elapsed=1000):
        return dict(id=identifier, answerable=answerable, classes=['routine'], outcome=outcome,
            answer=copy.deepcopy(answer) if outcome == 'completed' else None,
            errorCode=None if outcome == 'completed' else 'deadline_exceeded',
            terminalClientMs=elapsed if outcome == 'completed' else None,
            firstVisibleClientMs=100 if outcome == 'completed' else None)
    rows = [row('good'), row('late', elapsed=6001), row('failed', outcome='failed'),
            row('unknown', answerable=False), row('unknown-failed', answerable=False, outcome='failed')]
    document = dict(schemaVersion=1, kind='local-answer-capture', sourceUnchanged=True,
        questionCount=5, failures=2, questions=rows, corpusManifestSha256='fixture-corpus',
        inputSha256={'suite': 'fixture-suite'}, split='held_out', annotationStatus='fixture', settings={})
    review = scoring.review_template(document, 'fixture-capture')
    review.update(reviewStatus='fixture', reviewer='Synthetic fixture, not a human reviewer')
    for entry in review['questions']:
        entry.update(correct=True, complete=True, abstained=entry['id'] == 'unknown',
            factualClaims=0 if entry['id'] == 'unknown' else 1,
            supportedFactualClaims=0 if entry['id'] == 'unknown' else 1,
            citedFactualClaims=0 if entry['id'] == 'unknown' else 1,
            citations=0 if entry['id'] == 'unknown' else 1,
            supportedCitations=0 if entry['id'] == 'unknown' else 1)
    return document, review


class AnswerEvaluation(unittest.TestCase):
    def test_post_selection_regression_cannot_receive_held_out_credit(self):
        document, review = records()
        document['annotationStatus'] = 'human_reviewed'
        review.update(reviewStatus='human_reviewed', reviewer='Synthetic human-provenance fixture')
        original = scoring.score_capture(document, review, 'fixture-capture', 'fixture-review')
        self.assertTrue(original['reviewedHeldOut'])
        self.assertTrue(original['humanReviewedHeldOut'])
        review['evaluationRole'] = 'post_selection_regression'
        result = scoring.score_capture(document, review, 'fixture-capture', 'fixture-review')
        self.assertEqual(result['split'], 'held_out')
        self.assertEqual(result['evaluationRole'], 'post_selection_regression')
        self.assertFalse(result['reviewedHeldOut'])
        self.assertFalse(result['humanReviewedHeldOut'])
        self.assertEqual(result['metrics'], original['metrics'])
        for role in ['tuning', 'fresh_held_out', None]:
            review['evaluationRole'] = role
            with self.subTest(role=role), self.assertRaises(ValueError):
                scoring.score_capture(document, review, 'fixture-capture', 'fixture-review')

    def test_grouped_citations_preserve_delivered_text_and_reject_unknown_members(self):
        for text, valid in [('Use Redis [S1, S1].', True), ('Use Redis [S1,S2].', False),
                            ('Use Redis [S1,].', False), ('Use Redis [S1-S2].', False)]:
            files, settings, envelopes = stream()
            envelopes[2]['data']['text'] = text
            envelopes[-1]['data']['answer']['text'] = text
            trace = capture.AnswerTrace(files, settings)
            if valid:
                for response in envelopes: trace.observe(response)
                self.assertEqual(trace.answer['text'], text)
                self.assertEqual(len(trace.answer['citations']), 1)
            else:
                with self.subTest(text=text), self.assertRaises(ValueError):
                    for response in envelopes: trace.observe(response)

    def test_agent_review_preserves_provenance_and_checks_source_ledger(self):
        document, _ = records()
        document.update(questionCount=1, failures=0, annotationStatus='agent_reviewed')
        document['questions'] = document['questions'][:1]
        document['questions'][0]['events'] = stream()[2]
        review = scoring.review_template(document, 'fixture-capture', 'agent')
        review.update(reviewStatus='agent_reviewed', reviewer='Synthetic agent source-audit fixture', sourceAuditSha256='a'*64)
        review['questions'][0].update(correct=True, complete=True, abstained=False,
            factualClaims=1, supportedFactualClaims=1, citedFactualClaims=1, citations=1, supportedCitations=1,
            claimAssessments=[dict(id='c1', text='Redis is required.', supported=True, cited=True,
                reason='Exact supplied policy.', supportQuotes=[dict(label='S1', quote='Redis is required.')])],
            citationAssessments=[dict(label='S1', claimIds=['c1'], supported=True, reason='Entails the attached policy claim.')])
        result = scoring.score_capture(document, review, 'fixture-capture', 'fixture-review')
        self.assertEqual(result['reviewerKind'], 'agent')
        self.assertTrue(result['reviewedHeldOut'])
        self.assertFalse(result['humanReviewedHeldOut'])
        self.assertFalse(result['releaseQualified'])
        for mutation in ['provenance', 'digest', 'counts', 'quote', 'citation', 'coverage']:
            damaged = copy.deepcopy(review)
            entry = damaged['questions'][0]
            if mutation == 'provenance': damaged['reviewStatus'] = 'human_reviewed'
            if mutation == 'digest': damaged['sourceAuditSha256'] = 'not-a-digest'
            if mutation == 'counts': entry['factualClaims'] = 2
            if mutation == 'quote': entry['claimAssessments'][0]['supportQuotes'][0]['quote'] = 'Invented policy.'
            if mutation == 'citation': entry['citationAssessments'] = []
            if mutation == 'coverage': entry['claimAssessments'][0]['cited'] = False
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                scoring.score_capture(document, damaged, 'fixture-capture', 'fixture-review')

    def test_capture_checks_exact_evidence_stream_identity_and_all_token_counts(self):
        files, settings, responses = stream()
        trace = capture.AnswerTrace(files, settings)
        for response in responses:
            trace.observe(response)
        self.assertEqual(trace.answer['hiddenTokens'], 2)
        self.assertIsNotNone(trace.first_visible_ms)
        with self.assertRaises(ValueError):
            trace.observe(responses[-1])
        for mutation in ('sequence', 'revision', 'text', 'citation', 'tokens', 'budget', 'finish', 'nan'):
            damaged = copy.deepcopy(responses)
            if mutation == 'sequence': damaged[1]['data']['sequence'] = 9
            if mutation == 'revision': damaged[1]['data']['pack']['evidence'][0]['reference']['contentHash'] = 'wrong'
            if mutation == 'text': damaged[-1]['data']['answer']['text'] = 'different'
            if mutation == 'citation': damaged[-1]['data']['answer']['citations'][0]['label'] = 'S2'
            if mutation == 'tokens': damaged[-1]['data']['answer']['hiddenTokens'] = 100
            if mutation == 'budget': damaged[1]['data']['pack']['tokenCount'] = 101
            if mutation == 'finish': damaged[-1]['data']['answer']['finishReason'] = 'output_limit'
            if mutation == 'nan': damaged[-1]['data']['answer']['timingsMs']['total'] = float('nan')
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                trace = capture.AnswerTrace(files, settings)
                for response in damaged: trace.observe(response)

    def test_failures_stay_in_correctness_abstention_and_joint_denominators(self):
        document, review = records()
        result = scoring.score_capture(document, review, 'fixture-capture', 'fixture-review', allow_fixture=True)
        metrics = result['metrics']
        self.assertEqual(metrics['correctCompleteRate'], 2/3)
        self.assertEqual(metrics['correctAbstentionRate'], 1/2)
        self.assertEqual(metrics['jointSuccessWithin6s'], 1/3)
        self.assertEqual(metrics['supportedClaimRate'], 1)
        self.assertEqual(metrics['completedOnlyClientLatencyMs']['terminal']['samples'], 3)
        self.assertEqual(metrics['completedOnlyClientLatencyMs']['terminal']['p95'], 6001)
        self.assertFalse(result['reviewedHeldOut'])
        self.assertFalse(result['releaseQualified'])

    def test_review_requires_hash_binding_every_completed_row_and_real_judgments(self):
        document, review = records()
        for mutation in ('hash', 'missing', 'duplicate', 'failed', 'unreviewed', 'unknown', 'counts', 'bool'):
            damaged = copy.deepcopy(review)
            if mutation == 'hash': damaged['captureSha256'] = 'wrong'
            if mutation == 'missing': damaged['questions'].pop()
            if mutation == 'duplicate': damaged['questions'].append(damaged['questions'][0])
            if mutation == 'failed': damaged['questions'].append(dict(damaged['questions'][0], id='failed'))
            if mutation == 'unreviewed': damaged['reviewStatus'] = 'unreviewed'
            if mutation == 'unknown': damaged['questions'][0]['correct'] = None
            if mutation == 'counts': damaged['questions'][0]['supportedFactualClaims'] = 2
            if mutation == 'bool': damaged['questions'][0]['factualClaims'] = True
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                scoring.validate_review(document, damaged, 'fixture-capture', allow_fixture=True)
        with self.assertRaises(ValueError):
            scoring.validate_review(document, review, 'fixture-capture')

    def test_zero_claim_denominators_are_unknown_and_no_failures_become_successes(self):
        document, review = records()
        document['questions'] = [document['questions'][-1]]
        document.update(questionCount=1, failures=1)
        review['questions'] = []
        result = scoring.score_capture(document, review, 'fixture-capture', 'fixture-review', allow_fixture=True)
        self.assertIsNone(result['metrics']['correctCompleteRate'])
        self.assertIsNone(result['metrics']['supportedClaimRate'])
        self.assertEqual(result['metrics']['correctAbstentionRate'], 0)
        self.assertFalse(any(result['observedQualityThresholdChecks'].values()))

    def test_private_capture_is_exclusive_and_review_template_is_unreviewed(self):
        document, _ = records()
        template = scoring.review_template(document, 'fixture-capture')
        self.assertEqual(template['reviewStatus'], 'unreviewed')
        self.assertIsNone(template['reviewer'])
        self.assertTrue(all(entry['correct'] is None for entry in template['questions']))
        with tempfile.TemporaryDirectory(prefix='know-things-answer-review-') as temporary:
            path = Path(temporary)/'private.json'
            old = os.umask(0)
            try: capture.write_new(path, document)
            finally: os.umask(old)
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(FileExistsError): capture.write_new(path, {'overwrite': True})
            self.assertEqual(json.loads(path.read_text()), document)


if __name__ == '__main__':
    unittest.main()
