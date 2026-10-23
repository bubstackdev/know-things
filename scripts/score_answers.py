#!/usr/bin/env python3
"""Prepare or score provenance-explicit, hash-bound source reviews of local answers."""
import argparse
import json
import math
from pathlib import Path
from evaluate_answers import digest, finite_nonnegative, write_new
from citations import citation_occurrences

RUBRIC = 'know-things-human-answer-v1'
AGENT_RUBRIC = 'know-things-agent-source-answer-v1'
FLAGS = ('correct', 'complete', 'abstained')
COUNTS = ('factualClaims', 'supportedFactualClaims', 'citedFactualClaims', 'citations', 'supportedCitations')


def capture_rows(capture):
    if (capture.get('schemaVersion') != 1 or capture.get('kind') != 'local-answer-capture'
            or capture.get('sourceUnchanged') is not True):
        raise ValueError('Unsupported or unverified answer capture')
    rows = capture.get('questions', [])
    ids = {row['id'] for row in rows}
    if not rows or len(ids) != len(rows) or capture['questionCount'] != len(rows):
        raise ValueError('Capture requires unique, complete question rows')
    for row in rows:
        if type(row['answerable']) is not bool or row['outcome'] not in ('completed', 'failed'):
            raise ValueError('Invalid answer outcome/answerability')
        if not row['classes'] or len(set(row['classes'])) != len(row['classes']):
            raise ValueError('Invalid question classes')
        completed = row['outcome'] == 'completed'
        if completed != (row['answer'] is not None) or completed == bool(row['errorCode']):
            raise ValueError('Outcome, error, and answer disagree')
        if completed and (not finite_nonnegative(row['terminalClientMs'])
                or not finite_nonnegative(row['firstVisibleClientMs'])
                or row['firstVisibleClientMs'] > row['terminalClientMs']):
            raise ValueError('Completed answer needs valid observed timings')
        if completed and (row['answer']['finishReason'] not in ('stop', 'insufficient_evidence')
                or row['answer']['semanticValidation'] != 'not_evaluated'
                or row['answer']['referenceValidation'] not in ('valid', 'not_applicable')):
            raise ValueError('Completed answer lacks required mechanical checks')
    if capture['failures'] != sum(row['outcome'] != 'completed' for row in rows):
        raise ValueError('Failure denominator mismatch')
    return rows


def review_template(capture, capture_sha256, reviewer_kind='human'):
    rows = capture_rows(capture)
    if reviewer_kind not in {'human', 'agent'}:
        raise ValueError('Unknown reviewer kind')
    return dict(schemaVersion=1, kind=f'{reviewer_kind}-answer-review', rubric=AGENT_RUBRIC if reviewer_kind == 'agent' else RUBRIC,
        captureSha256=capture_sha256, reviewStatus='unreviewed', reviewer=None,
        evaluationRole=capture['split'],
        instructions='Review correctness, all required claims, conflicts, abstention, and every factual claim/citation against the pinned evidence. Fill all fields for completed rows; failed rows receive no credit and need no judgment. Record agent_reviewed for an agent source audit, or human_reviewed only for an actual human review; preserve source evidence and review reasoning.',
        questions=[dict(id=row['id'], **{key: None for key in FLAGS+COUNTS})
                   for row in rows if row['outcome'] == 'completed'])


def validate_review(capture, review, capture_sha256, allow_fixture=False):
    rows = capture_rows(capture)
    expected = {'human-answer-review': (RUBRIC, 'human_reviewed'),
                'agent-answer-review': (AGENT_RUBRIC, 'agent_reviewed')}.get(review.get('kind'))
    if (review.get('schemaVersion') != 1 or expected is None
            or review.get('rubric') != expected[0] or review.get('captureSha256') != capture_sha256):
        raise ValueError('Review does not bind this capture/rubric')
    if review.get('reviewStatus') != expected[1] and not (allow_fixture and review.get('reviewStatus') == 'fixture'):
        raise ValueError('A completed review with matching reviewer provenance is required; fixtures need --allow-fixture')
    if review.get('reviewStatus') == 'agent_reviewed':
        audit = review.get('sourceAuditSha256', '')
        if not isinstance(audit, str) or len(audit) != 64 or any(c not in '0123456789abcdef' for c in audit):
            raise ValueError('Agent reviews require a source-audit digest')
    if not isinstance(review.get('reviewer'), str) or not review['reviewer'].strip():
        raise ValueError('Reviewer provenance is required')
    role = review.get('evaluationRole', capture['split'])
    if role not in {capture['split'], 'post_selection_regression'}:
        raise ValueError('Evaluation role must match the original split or declare post-selection regression')
    entries = review.get('questions', [])
    judgments = {entry['id']: entry for entry in entries}
    if len(judgments) != len(entries) or judgments.keys() != {row['id'] for row in rows if row['outcome'] == 'completed'}:
        raise ValueError('Review must cover every completed answer exactly once')
    for entry in entries:
        if any(type(entry.get(key)) is not bool for key in FLAGS):
            raise ValueError('Review judgments must be explicit booleans')
        if any(type(entry.get(key)) is not int or not 0 <= entry[key] <= 100000 for key in COUNTS):
            raise ValueError('Review claim/citation counts must be bounded nonnegative integers')
        if (entry['supportedFactualClaims'] > entry['factualClaims']
                or entry['citedFactualClaims'] > entry['factualClaims']
                or entry['supportedCitations'] > entry['citations']):
            raise ValueError('Supported/cited counts cannot exceed their denominators')
        if review.get('reviewStatus') == 'agent_reviewed':
            validate_agent_ledger(next(row for row in rows if row['id'] == entry['id']), entry)
    return rows, judgments


def validate_agent_ledger(row, entry):
    """Verify audit accounting and quoted evidence, without pretending to judge entailment."""
    claims, citations = entry.get('claimAssessments'), entry.get('citationAssessments')
    if (not isinstance(claims, list) or not isinstance(citations, list)
            or any(not isinstance(item, dict) for item in claims+citations)):
        raise ValueError('Agent reviews require claim and citation ledgers')
    indexed = {claim.get('id'): claim for claim in claims}
    if (len(indexed) != len(claims) or any(not isinstance(key, str) or not key for key in indexed)
            or any(not isinstance(claim.get('text'), str) or not claim['text'].strip()
                   or type(claim.get('supported')) is not bool or type(claim.get('cited')) is not bool
                   or not isinstance(claim.get('reason'), str) or not claim['reason'].strip() for claim in claims)):
        raise ValueError('Invalid agent claim ledger')
    pack = next((event['data']['pack'] for event in row.get('events', [])
                 if event.get('data', {}).get('event') == 'evidence_ready'), None)
    evidence = {f'S{i+1}': item['text'] for i, item in enumerate(pack['evidence'])} if pack else {}
    support = {}
    for claim in claims:
        quotes = claim.get('supportQuotes', [])
        if not isinstance(quotes, list) or (claim['supported'] and not quotes):
            raise ValueError('Supported agent claims require quoted supplied evidence')
        support[claim['id']] = set()
        for quote in quotes:
            label, text = quote.get('label'), quote.get('quote')
            if not isinstance(text, str) or not text.strip() or label not in evidence or text not in evidence[label]:
                raise ValueError('Agent support quote differs from supplied evidence')
            support[claim['id']].add(label)
    occurrences = citation_occurrences(row['answer']['text'], len(evidence))
    if [item.get('label') for item in citations] != occurrences:
        raise ValueError('Agent citation ledger must cover every citation occurrence in order')
    attached = set()
    for citation in citations:
        ids = citation.get('claimIds')
        if (not isinstance(ids, list) or not ids or len(set(ids)) != len(ids) or any(key not in indexed for key in ids)
                or type(citation.get('supported')) is not bool or not citation.get('reason')):
            raise ValueError('Invalid agent citation assessment')
        attached.update(ids)
        if citation['supported'] and not any(indexed[key]['supported'] and citation['label'] in support[key] for key in ids):
            raise ValueError('Supported citation requires matching claim evidence')
    if any(claim['cited'] != (claim['id'] in attached) for claim in claims):
        raise ValueError('Agent citation attachment differs from claim coverage')
    actual = dict(factualClaims=len(claims), supportedFactualClaims=sum(c['supported'] for c in claims),
                  citedFactualClaims=sum(c['cited'] for c in claims), citations=len(citations),
                  supportedCitations=sum(c['supported'] for c in citations))
    if any(entry[key] != value for key, value in actual.items()):
        raise ValueError('Agent ledger counts differ from declared review counts')


def ratio(numerator, denominator):
    return numerator/denominator if denominator else None


def percentile(values, fraction):
    # Nearest-rank quantiles; no fabricated timings for failures/unobserved text.
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered)*fraction)-1)] if ordered else None


def aggregate(rows, judgments):
    answerable = [row for row in rows if row['answerable']]
    unknown = [row for row in rows if not row['answerable']]
    completed = [row for row in rows if row['outcome'] == 'completed']
    def correct_complete(row):
        entry = judgments.get(row['id'])
        return bool(entry and entry['correct'] and entry['complete'] and not entry['abstained'])
    def joint(row):
        entry = judgments.get(row['id'])
        return bool(correct_complete(row) and row['terminalClientMs'] <= 6000
            and row['answer']['referenceValidation'] == 'valid'
            and entry['factualClaims'] > 0 and entry['citations'] > 0
            and entry['supportedFactualClaims']/entry['factualClaims'] >= .98
            and entry['citedFactualClaims']/entry['factualClaims'] >= .95
            and entry['supportedCitations']/entry['citations'] >= .98)
    totals = {key: sum(judgments[row['id']][key] for row in completed) for key in COUNTS}
    first = [row['firstVisibleClientMs'] for row in completed]
    terminal = [row['terminalClientMs'] for row in completed]
    return dict(questionCount=len(rows), failures=len(rows)-len(completed), completedCount=len(completed),
        answerableCount=len(answerable), unanswerableCount=len(unknown),
        correctCompleteRate=ratio(sum(correct_complete(row) for row in answerable), len(answerable)),
        supportedClaimRate=ratio(totals['supportedFactualClaims'], totals['factualClaims']),
        citationPrecision=ratio(totals['supportedCitations'], totals['citations']),
        factualClaimCitationCoverage=ratio(totals['citedFactualClaims'], totals['factualClaims']),
        correctAbstentionRate=ratio(sum(bool(judgments.get(row['id'], {}).get('correct')
            and judgments[row['id']]['abstained']) for row in unknown), len(unknown)),
        inappropriateRefusalRate=ratio(sum(bool(judgments.get(row['id'], {}).get('abstained'))
            for row in answerable), len(answerable)),
        jointSuccessWithin6s=ratio(sum(joint(row) for row in answerable), len(answerable)),
        manualCounts=totals,
        completedOnlyClientLatencyMs={name: dict(p50=percentile(values, .5), p95=percentile(values, .95),
            p99=percentile(values, .99), samples=len(values)) for name, values in [('firstVisible', first), ('terminal', terminal)]})


def score_capture(capture, review, capture_sha256, review_sha256, allow_fixture=False):
    rows, judgments = validate_review(capture, review, capture_sha256, allow_fixture)
    overall = aggregate(rows, judgments)
    thresholds = {'correctCompleteRate': .90, 'supportedClaimRate': .98, 'citationPrecision': .98,
        'factualClaimCitationCoverage': .95, 'correctAbstentionRate': .95, 'jointSuccessWithin6s': .85}
    checks = {key: overall[key] is not None and overall[key] >= target for key, target in thresholds.items()}
    checks['inappropriateRefusalRate'] = overall['inappropriateRefusalRate'] is not None and overall['inappropriateRefusalRate'] <= .05
    reviewed_statuses = {'human_reviewed', 'agent_reviewed'}
    role = review.get('evaluationRole', capture['split'])
    return dict(schemaVersion=1, kind='reviewed-local-answer-evaluation', rubric=review['rubric'],
        captureSha256=capture_sha256, reviewSha256=review_sha256,
        sourceAuditSha256=review.get('sourceAuditSha256'), reviewedSuiteSha256=review.get('reviewedSuiteSha256'),
        corpusManifestSha256=capture['corpusManifestSha256'], inputSha256=capture['inputSha256'],
        split=capture['split'], evaluationRole=role,
        annotationStatus=capture['annotationStatus'], reviewStatus=review['reviewStatus'],
        reviewer=review['reviewer'], reviewerKind='agent' if review['reviewStatus'] == 'agent_reviewed' else 'human' if review['reviewStatus'] == 'human_reviewed' else 'fixture', settings=capture['settings'], metrics=overall,
        byClass={label: aggregate([row for row in rows if label in row['classes']], judgments)
                 for label in sorted({label for row in rows for label in row['classes']})},
        observedQualityThresholdChecks=checks,
        reviewedHeldOut=role == 'held_out' and capture['split'] == 'held_out' and capture['annotationStatus'] in reviewed_statuses
            and review['reviewStatus'] in reviewed_statuses,
        humanReviewedHeldOut=role == 'held_out' and capture['split'] == 'held_out' and capture['annotationStatus'] == 'human_reviewed'
            and review['reviewStatus'] == 'human_reviewed', releaseQualified=False,
        limitations=['Reviewer/annotation provenance is declared, not independently certified',
            'Failures/timeouts stay in answerable, abstention, and joint-success denominators',
            'Claim/citation rates use reviewer-counted micro denominators; zero denominators are null',
            'Latency quantiles include completed requests only; inspect failures and sample counts',
            'Client timing includes IPC/validation; no isolated-contention or hardware-tier claim',
            'Quality thresholds are diagnostics for this run, not a release or competitive qualification',
            'Direct source review is declared provenance; agent and human reviews are distinguished',
            'Evaluation role is declared; post-selection regression does not qualify as reviewed held-out evidence',
            'Agent source review is not an independent human evaluation or calibrated local judge'])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=['template', 'score'])
    parser.add_argument('--capture', type=Path, required=True)
    parser.add_argument('--review', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--allow-fixture', action='store_true')
    parser.add_argument('--reviewer-kind', choices=['human', 'agent'], default='human', help='Template provenance; scoring uses the review itself')
    args = parser.parse_args()
    if args.output.exists():
        parser.error('Output must be new')
    raw = args.capture.read_bytes()
    capture, identity = json.loads(raw), digest(raw)
    if args.operation == 'template':
        if args.review or args.allow_fixture:
            parser.error('Template creation does not accept a review')
        result = review_template(capture, identity, args.reviewer_kind)
    else:
        if args.review is None:
            parser.error('Scoring requires --review')
        raw_review = args.review.read_bytes()
        result = score_capture(capture, json.loads(raw_review), identity, digest(raw_review), args.allow_fixture)
    write_new(args.output, result)
    print(json.dumps(dict(kind=result['kind'], releaseQualified=False)))


if __name__ == '__main__':
    main()
