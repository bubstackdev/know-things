# Reproducible evaluation

`scripts/evaluate_retrieval.py` runs local retrieval against revision-pinned labels.
It validates the corpus manifest and every supporting span before import, creates a
new disposable store outside the read-only source, and verifies returned text against
the declared bytes. No model acquisition or remote judge occurs in this script.

Keep actual questions, required claims, and source excerpts under `.private/`.
The output summary omits queries, claim text, source paths, and excerpts. It records
the corpus/suite/binary hashes, annotation provenance, selected split, budgets, actual
retrieval modes, model-space IDs, per-question failures, and mechanical scores.
Store and summary destinations must be new; historical evidence is never overwritten.

A suite has this shape (offsets below describe the synthetic bytes `Policy.`):

```json
{
  "schemaVersion": 1,
  "corpusManifestSha256": "HASH_FROM_CORPUS_MANIFEST",
  "annotationStatus": "agent_draft",
  "questions": [
    {
      "id": "q001",
      "split": "tuning",
      "classes": ["paraphrase"],
      "query": "What policy applies?",
      "answerable": true,
      "claims": [
        {
          "text": "The required source-grounded claim",
          "support": [
            {"path": "guide.md", "contentHash": "SHA256_OF_COMPLETE_FILE", "byteStart": 0, "byteEnd": 7}
          ]
        }
      ]
    }
  ]
}
```

Offsets are half-open UTF-8 byte spans in the complete hashed file. Every support span
for a claim is required; evidence from adjacent/overlapping retrieved spans can jointly
cover it. Use multiple claims for multi-source answers. Unanswerable questions have
`answerable: false` and an empty `claims` array. They require separate answer/abstention
evaluation and are excluded from retrieval-recall denominators.

`annotationStatus` is `agent_draft`, `fixture`, `agent_reviewed`, or `human_reviewed`.
Both reviewed statuses require a named `reviewer`; agent review additionally binds a
`sourceAuditSha256`. These are explicit provenance declarations, not independent
certification. Draft/fixture runs require `--allow-draft`. Review entailment, question
scope, supporting spans, omissions, conflicts, and answerability before promotion.
Do not rename drafts to manufacture gold data or label an agent audit as human work.

Questions used during selection become post-selection regression data. Record
`evaluationRole: "post_selection_regression"` in their hash-bound review. The scorer
preserves the original split but does not promote such runs to fresh held-out evidence.
Record agent and human review provenance accurately; neither declaration establishes
independence by itself. Source details and case-specific research remain private.

```sh
python3 scripts/evaluate_retrieval.py --binary .private/install/bin/know-things --source /path/to/docs --manifest .private/evaluation/corpus.manifest.json --suite .private/evaluation/questions.json --store .private/evaluation/run-001 --output .private/evaluation/result-001.json --mode lexical --split tuning --allow-draft
```

For semantic qualification, select `--mode hybrid --embedding-bundle BUNDLE`.
Hybrid is strict by default; `--allow-degraded` explicitly permits and records lexical
fallback. Missing semantic capability produces failure rows and zero answerable recall,
rather than silently changing the experiment. Any failed query makes the command exit
with code 2 after saving its report. Import failures, corrupt references, binary/source
changes, and invalid labels abort without publishing scores. Transport failure leaves
remaining questions as failures, preserving their denominators.

Scores are macro averages across answerable questions, with separate query-class slices:

- `claimRecallAt5/10/20`: fraction of required claims whose entire support is covered.
- `supportSpanRecallAt5/10/20`: fraction of required supporting spans fully covered.
- `completeEvidenceAt20`: fraction of questions with every required claim covered.
- `firstSupportReciprocalRankAt20`: reciprocal of the first rank where any full supporting
  span is covered, zero when none is. This measures first supporting evidence, not a
  correct complete answer.

These are evidence-coverage metrics, not entailment judgments, calibrated confidence,
standard document-relevance nDCG, or answer-quality scores. nDCG requires independently
graded ranking judgments and remains pending. Client wall times include reference
verification; they are not published as isolated retrieval-stage latency. Use the
separate smoke/timing harness for diagnostic timings and the broader validation plan
for qualified latency/resource measurements.

Freeze the suite and split before tuning. Use tuning results for implementation work;
reserve held-out questions for declared profile comparisons. Comparative runs need
identical suite/corpus hashes, scope, budgets, and units. Competitive adapters, semantic judgments, and reviewed answer evaluation
remain release work. `releaseQualified` remains false in these partial retrieval reports.

Run the evaluator's correctness and installed-interface checks with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 tests/evaluation.py --binary .private/install/bin/know-things
```

## Paired comparisons

`compare_evaluations.py` matches questions by ID and refuses mismatched suite/corpus,
split, annotation provenance, evidence budget/tokenizer, or query deadline. Failed
answerable queries stay in the pairs with zero recall; unanswerable queries are excluded.
It reports candidate-minus-baseline means and deterministic 95% percentile-bootstrap
intervals by resampling questions, plus class slices. Shared-document and paraphrase
correlations are not modeled; small slices warrant caution. This measures a declared
metric difference, not a competitive win or release gate by itself.

```sh
python3 scripts/compare_evaluations.py --baseline baseline.json --candidate candidate.json --output comparison.json --metric claimRecallAt20 --samples 10000 --seed 20261022
```

A comparison service must expose the same source snapshot and evidence budget to
support controlled paired scoring. Keep unmatched service observations private and
label their scope accurately.

## Answer capture and source review

`scripts/evaluate_answers.py` captures local streamed answers using the same frozen
suite, split, source manifest, and exact-span validation. It requires a new disposable
store and a new capture file outside the read-only source tree. Explicit asset flags
select verified local embedding/Flash/profile bundles; a packaged launcher may supply
its installed assets instead. No acquisition or inference judge runs in this harness.

```sh
python3 scripts/evaluate_answers.py --binary /path/to/know-things --source /path/to/docs --manifest .private/evaluation/corpus.manifest.json --suite .private/evaluation/questions.json --store .private/evaluation/answer-store-001 --output .private/evaluation/answer-capture-001.json --mode hybrid --embedding-bundle /path/to/embedding --flash-bundle /path/to/flash --generation-profile /path/to/profile --profile PROFILE_NAME --split held_out --evidence-budget 3000 --input-budget 4096 --output-budget 1024 --deadline-ms 30000
```

Draft/fixture labels require `--allow-draft`; hybrid is strict unless
`--allow-degraded` is explicit. Do not execute the actual held-out split for tuning.
Use reviewed labels with explicit audit provenance, retaining original drafts and
previous captures privately. See [validation](VALIDATION.md) for qualification limits.

The capture records input/script and selected bundle-manifest hashes, settings,
question order, every observed event, answer/profile/token/stage metrics, required
claims and supporting bytes, selected evidence, client first-visible/terminal timing,
and failures. It checks stream sequence/run identity, byte-exact evidence, request
budgets, completed/delivered text equality, and citation labels/references. Every
completed citation is resolved through `context` and checked against the delivered
evidence. Policy abstentions have zero generated tokens and separately tokenized
visible policy text; they are retained for source-grounded answerability review.

The answer default is 1024 output tokens. Its selected profile must fit both input
and output allowances; new dense candidate bundles default to 8192 context tokens.
Keep budget changes explicit in the capture and retain truncated outcomes as failures.
`--generation-startup-deadline-ms` accepts 1000–900000 ms and forwards the core's
separate initialization setting; first status/startup is measured before import.
It does not enlarge answer deadlines. Prepared local Q4 assets avoid repeated BF16
conversion, but distinct quantizers require distinct profiles and qualification.

On macOS, `--monitor-system-pressure` performs a normal-pressure preflight and probes
once per second. Warning, unknown state, or probe timeout terminates only the harness's
owned coordinator. The separate private `.pressure.json` receipt records its trigger;
subsequent unavailable requests stay failed. It does not stop or inspect other sessions.
An external `sandbox-exec` network-denial wrapper is recorded separately; the evaluator
itself does not impose an OS network policy.

Requests run sequentially in one new resident process, one sample per question.
Startup/import precede request timing. First-use effects remain in the first request;
later requests retain their suite order. Client timing includes IPC and validation
callbacks but excludes subsequent citation-context verification. No contention,
tail-latency, hardware-tier, or independent OS network-denial qualification is inferred.
Source and tracked-input hashes are checked again before publication. Import/verification
failure publishes no capture; admission/inference failures become rows and command
exit 2. Transport loss marks all remaining questions failed without restarting/replaying.

The explicit capture contains private questions, claims, source text, and answers.
It is created exclusively with mode 0600, including under a permissive umask. Keep it
under `.private/`; it is not a sanitized public evaluation summary. No automatic query
logging or retention is enabled in the product by running this separate harness.

Prepare a separate hash-bound review template (`--reviewer-kind agent` selects agent provenance):

```sh
python3 scripts/score_answers.py template --capture .private/evaluation/answer-capture-001.json --output .private/evaluation/answer-review-001.json
```

The template starts `unreviewed`, with no reviewer and null judgments. The declared reviewer
checks each completed answer against the required claims and original supporting
bytes, including omitted facts, conflicts, unsupported additions, citations, and
answerability. Fill explicit `correct`, `complete`, and `abstained` booleans. Count all
`factualClaims`, those supported by the evidence (`supportedFactualClaims`), and those
with a citation (`citedFactualClaims`). Count citation units under the declared rubric: the agent rubric counts every source-label
occurrence, including members of grouped citations; the legacy human rubric counts claim-to-evidence citation links.
Record which are supported (`supportedCitations`). Both can differ from the coordinator's
unique source-label list; do not silently compare different counting rubrics. Notes remain
private. Failed requests need no judgment and cannot earn credit. After the actual source
review, declare `agent_reviewed` or `human_reviewed` to match the reviewer, and record
the reviewer name and required audit evidence.
This declaration records provenance; the scorer does not independently certify it.

```sh
python3 scripts/score_answers.py score --capture .private/evaluation/answer-capture-001.json --review .private/evaluation/answer-review-001.json --output .private/evaluation/answer-score-001.json
```

The scorer rejects a wrong capture hash, duplicate/missing/extra judgments, null flags,
or invalid counts. Correct-and-complete and joint-success denominators include every
answerable request; correct abstention includes every unanswerable request. Failures
and timeouts receive no success credit. Inappropriate refusals are completed abstentions
on answerable questions; failed requests remain visible separately and fail correctness.
Supported-claim, citation-precision, and factual-citation-coverage rates use micro counts
over reviewed completed outputs; a zero denominator is `null`, never automatic success.

Joint success requires a correct, complete, non-abstaining answer delivered within six
seconds, valid mechanical references, supported factual claims and citation precision
at least 98%, and factual citation coverage at least 95% for that answer. Completed-only
client latency quantiles include sample counts and exclude failed/unobserved timings;
they do not conceal failures from quality/joint-success denominators. Class slices and
observed quality-threshold checks are diagnostics for the captured run. The score
omits queries, source paths/excerpts, answers, and review notes. Both scripts always
leave `releaseQualified: false`: actual labels, profiles, hardware, competitive results,
latency/resource scope, and the full validation gates still need qualification.

Synthetic reviews require `reviewStatus: "fixture"` plus `--allow-fixture` and never
produce `reviewedHeldOut: true`. They test scoring mechanics, not answer quality.
Six pure Python checks and a disposable CLI/native-protocol fixture cover failures,
denominators, hash binding, private output, citation resolution, local abstention, and
the template/scorer workflow:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 tests/answer_evaluation.py
cargo test --locked --offline --test answer_interfaces answer_evaluation_captures_real_interface_citations_and_local_abstention
```

### Agent source review accounting

Agent reviews use `kind: agent-answer-review`, rubric
`know-things-agent-source-answer-v1`, `reviewStatus: agent_reviewed`, a reviewer name,
and the source-audit SHA-256. They include a claim ledger with explicit supported/cited
flags and verbatim support quotes from supplied source labels, plus a citation ledger
covering every source-label occurrence in response order, expanding comma-separated
groups such as `[S1, S2]` into two occurrences. The scorer verifies quote
membership, attachments, declared counts, and reviewer provenance. Entailment is the
agent's recorded source judgment; quote membership alone does not certify it. Historical
`human-answer-review` and explicit synthetic fixture workflows remain supported.

Use unique independently checkable factual/policy assertions; deduplicate repeated
paraphrases and record the chosen granularity. A non-abstaining answer can cover every
required fact yet fail correctness by adding an unsupported requirement. Citation
precision counts source-label occurrences, including repeated labels. The legacy metric
field `manualCounts` contains reviewer-counted totals for both agent and human audits.

`reviewedHeldOut` means the frozen labels and answer review both have a reviewed status;
`humanReviewedHeldOut` requires actual human provenance for both. `reviewerKind` always
distinguishes agent, human, and fixture. Neither flag automatically qualifies a release,
independence, a model family, or a hardware/performance tier. No cloud inference fallback
or runtime semantic validator is added; captured answers retain `semanticValidation:
not_evaluated` and the separate audit supplies evaluation judgments.
