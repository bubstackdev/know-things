# Validation and delivery

The retrieval foundation, streaming answer coordinator and local model integration
are implemented. Product and release qualification remain unfinished.

## Observed local result

An [anonymous aggregate summary](evaluations/local-validation-summary.json) records two
complete 53-question runs on an Apple M5 Max with 128 GiB RAM. Each run answered all
47 answerable questions correctly and completely under agent source review and correctly
abstained on all six unknowns. First-visible p95 was **1873.78 ms and 1874.61 ms**;
terminal p95 was 4.85 and 4.70 seconds. Every answerable response completed correctly
within six seconds. The first run had a 2.09-second outlier; the repeat's maximum was
1.97 seconds. This establishes a p95 observation, not a per-request guarantee.

The runs used the same E5 embedding model and 27B affine-Q4 generation payload, an
800-token evidence budget, 3840 input tokens, 1024 output tokens and a 30-second answer
deadline. The selected captured engine used 1024-token dense prefill batches. A bounded
static system-instruction prefix was prepared at worker startup. Model and tokenizer
identities remain explicit in private qualification records.

Timings cover resident-worker requests, including retrieval, source/question prefill,
cache restore and first visible delivery. Startup and import are separate. The workload
was previously observed and source-reviewed by an agent, so it is post-selection
regression evidence rather than independent or fresh held-out validation. Shared-host
measurements establish no isolated causal speedup or supported hardware tier.

Private source manifests, internal names and links, source/citation hashes, questions,
answers, detailed receipts and historical research records are excluded from this
repository. Preserve them privately when reproducing or extending a local evaluation.

## Verification

- 139 Rust tests pass with the test workload serialized; Clippy reports no warnings.
- Evaluator, review-accounting, packaging and owned-worker pressure checks pass.
- Relocated local assets pass offline payload verification and three cited GPU fixtures.
- Active GPU cancellation fences visible output; a fresh worker prepares its prefix
  again and reproduces the clean baseline answer.
- Lifecycle checks use disposable sources. Actual documentation remains read-only.

Earlier parallel timing-test timeouts were retried by reducing this session's test
concurrency; assertions and deadlines were preserved. Never suspend or interfere with
other terminal sessions or their processes to isolate a measurement. Reduce or stop
only work owned by the current session when resources are contended.

## Acceptance and remaining gates

1. Retrieval: immutable references, declared lexical degradation, strict hybrid coverage,
   scope/exclusion correctness, and exact source resolution after supported lifecycle changes.
2. Answers: source-supported claims, complete requested facts, valid citations, explicit
   unknowns/conflicts, bounded streams, cancellation and coherent worker recovery.
3. Performance: declare request budgets, startup/cache regime and hardware. Count failures
   in quality denominators and retain every outlier and long output. Repeat complete suites.
4. Release: independent evaluation on new corpora, broader resource/pressure and crash
   coverage, supported hardware tiers, transferable installation tests and distribution
   licensing. No release or competitive qualification is claimed yet.

Model and backend changes require qualification. Add approximate search, reranking or
larger infrastructure only for a measured requirement and include recovery/upgrade paths.
See [architecture](ARCHITECTURE.md), [contracts](CONTRACTS.md) and the
[evaluation workflow](EVALUATION.md) for the governing rules.
