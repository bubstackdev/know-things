# Know-Things

On-device documentation retrieval and cited answer generation, under development.
Know-Things owns documentation; [knowcode](../knowcode) owns source-code understanding.
The accepted foundation is Rust, SQLite FTS5 plus exact vector retrieval, and a
persistent Flash generation worker.

The retrieval foundation and streaming answer coordinator are implemented, with real
on-device embedding/generation measurements and agent source-reviewed answers on the
initial corpus. Product qualification remains unfinished; no supported hardware tier
or competitive advantage is established. See [validation progress](docs/VALIDATION.md).

The current Rust build reaches **1.87-second p95 first-visible output** in two full
53-question runs. All 47 answerable questions are correct and complete under agent
source review, and all 6 unknowns abstain. It uses compact evidence, a static instruction
prefix prepared at startup and a pinned 1024-token Flash prefill batch. See the
[local validation](docs/VALIDATION.md)
for explicit settings, outliers and qualification limits.

## Build and run

Requires Rust 1.98+ on a Unix host. SQLite is bundled and pinned by `Cargo.lock`.

```sh
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Use `--offline` when dependencies are already cached. Start a coordinator on a
private data directory **outside** the source collection:

```sh
target/debug/know-things serve .private/data
```

A separate terminal can submit a request:

```sh
printf '%s\n' '{"schemaVersion":1,"requestId":"register","operation":"register","name":"kb","root":"/absolute/path/to/docs"}' | target/debug/know-things request .private/data
printf '%s\n' '{"schemaVersion":1,"requestId":"import","operation":"reconcile","collection":"kb"}' | target/debug/know-things request .private/data
printf '%s\n' '{"schemaVersion":1,"requestId":"search","operation":"search","collections":["kb"],"query":"Redis cache","mode":"lexical","topK":10,"evidenceTokenBudget":12000}' | target/debug/know-things request .private/data
```

`know-things local DATA_DIR` is a resident coordinator accepting one JSON request
per stdin line and returning JSON responses per stdout line. Answers stream multiple
responses with the same request ID; other operations return one response. Pipe its
stdout to a client or file; resident stdio modes reject terminal output. It supports restricted
environments without Unix socket binding. It owns the same exclusive store lock;
stop its own input stream before opening another coordinator on that directory.
Reconciliation runs alongside foreground requests, with one import at a time.
Responses may arrive out of order; match `requestId` and wait for reconciliation's
response before submitting a query that depends on its newly imported content.

All responses carry a version, request ID, status, duration, and corpus generation.
Operations currently include `register`, `collection_policy`, `set_collection_policy`, `reconcile`, `search`, `context`,
`exclude`, `include`, `remove_collection`, `prune`, `status`, `integrity`, `rebuild_lexical`, `backup`,
`answer`, and `cancel`. `know-things schema` generates the
operation JSON schema from the Rust contract types.

Hybrid searches without an embedding model explicitly report lexical degradation.
Set `strictHybrid: true` to require semantic coverage. Retrieval-only budgets use
the explicitly named `utf8-byte-v1` tokenizer (one token per UTF-8 byte), **not** a
generation tokenizer. A generation profile must revalidate evidence using its own
tokenizer before inference.

If an explicitly selected embedding bundle fails startup, lexical retrieval remains
available. `status.embeddingError` records the typed startup failure; permissive hybrid
queries report degradation and strict hybrid returns `model_unavailable`. Repair the
assets and restart the coordinator to enable the model. Invalid CLI options and store
failures still prevent startup.

Identical document text reuses embedding inference and storage within the same model
space, including across collections. Each source keeps independent references and
exclusion/removal behavior. Retained history owns its shared values until pruning
removes the last reference. Store schema 5 adds this sharing; back up before upgrading.
Older binaries refuse the new schema, so keep the prior binary and backup for rollback.

`context` takes the complete `reference` object returned by search. Its default
`scope: "exact"` returns that immutable span, including after an edit. `section`,
`neighbors`, and `document` scopes return bounded expansion with individually
resolvable references and truncation metadata; `evidenceTokenBudget` defaults to 3000. Excluded sources are unavailable
even when retained bytes exist. `exclude` takes a `document` ID and persists across
reconciliation. `include` clears an explicit exclusion and requires reconciliation
before the document is eligible again. `remove_collection` removes canonical records
without editing source files. `prune` takes `retainRevisions` (1–1000 per document),
optional `maxRevisions` (1–1000 removals per call), and `deadlineMs` (default 5000).
It retains the active revision first, then the newest revisions. Pending answers lease
their evidence until terminal delivery, cancellation/drop, or the answer deadline.
Pruning reports remaining/deferred revisions; repeat a bounded pass after readers
finish. Removed references become explicitly unavailable. These operations do not promise
secure erasure of underlying storage. Source edits/deletions are discovered by explicit `reconcile`
or opt-in native watching/periodic scans. Parsing and
document embeddings run outside the store lock; each document publishes atomically.
Foreground embeddings take priority between background embedding units. Large-corpus
concurrency and resource limits still require qualification.

Collection policies persist literal relative exclusions and an optional scan interval.
For example, register with `"policy":{"excludedPaths":["private","draft.md"],"reconcileIntervalMs":60000}`.
`private` excludes that path and its descendants; rules are case sensitive and do not
accept wildcards, absolute paths, or trailing slashes. Filtered files are skipped before
reading/parsing. Omit the policy when re-registering to preserve existing settings.
Read settings with `collection_policy`; replace them with `set_collection_policy`.
Tightening exclusions immediately hides affected sources and fences answer output;
relaxing them requires reconciliation. Explicit document exclusions persist independently.
The default `reconcileIntervalMs: null` disables scanning when watching is off; enabled values range from
1000 to 86400000 ms. One periodic worker shares the manual ingestion limit, skips busy
attempts, and reports metadata under `status.periodicReconciliation`.

Set `"watch":true` in a collection policy to enable native change hints on macOS.
For example, `"policy":{"watch":true,"excludedPaths":["private"]}` enables watching
while retaining that exclusion. Policy replacement replaces all settings; preserve
other exclusions/retention settings when enabling it. One owned maintenance worker
coalesces hints into normal reconciliation and performs recovery scans at an interval
of at most 60 seconds after the previous attempt; a shorter configured interval wins.
`watch` defaults to false, including after upgrade. With both watching and the scan
interval disabled, changes require explicit reconciliation.

The native backend watches at most 512 directories/document files across collections.
`status.periodicReconciliation.watch` reports active roots, target counts, event counts,
and errors. A watch limit, unavailable root, or unsupported platform leaves periodic
recovery available. Symlinks are rejected and exclusions are applied before traversal.
Watching requires store schema 6; back up before upgrading. Current-host fixture checks
do not establish a large-corpus or supported-hardware watch tier.

Automatic revision retention is also opt-in. Set a collection policy such as
`"retention":{"retainRevisions":2,"intervalMs":3600000}` to keep its active revision
and newest history. Defaults are at most 128 removals and 1000 ms per pass; optional
`maxRevisions` accepts 1–1000 and `deadlineMs` accepts 1–5000. Omit or set `retention`
to `null` when replacing policy to disable it. Cleanup resumes after restart, shares
the periodic worker, and runs independently of source availability. Its metadata is
under `status.periodicReconciliation.retention`. Retention preserves pending-answer
leases and may need multiple passes; it does not impose a hard disk-space cap.

## MCP

Connect an MCP stdio client to `know-things mcp DATA_DIR` to use an existing
socket coordinator. Alternatively, `know-things mcp-stdio DATA_DIR` owns a store
and runs directly as the client's resident subprocess. Both expose the same typed
operations through the `know_things` tool. Initialization and stdout contain only
protocol traffic; diagnostics use stderr. Answer progress is emitted when the client
supplies `_meta.progressToken`; the final tool result includes citations and retrieval
mode even without progress. `notifications/cancelled` cancels active answer tool calls.

## Offline embedding assets

Model acquisition is explicit. Download the pinned MiniLM candidate with:

```sh
python3 scripts/acquire_embedding.py --destination .private/minilm-source
```

This verifies weight SHA-256 and pinned Git objects; it requires network access.
The pinned MiniLM assets were acquired and verified on 2026-10-23 after network
access was enabled. A captured-runtime candidate is installed at
`.private/embedding-minilm-001` (384 dimensions, 256-token windows). It passes
operational retrieval/lifecycle checks with networking denied by the OS. Labeled
quality qualification remains open; query workers still load only local assets.

A second pinned acquisition candidate is available with
`--model intfloat/e5-base-v2`. Its bundle requires 768 dimensions, 512-token
windows, `--query-prefix 'query: '`, and `--document-prefix 'passage: '`.
Both candidates require corpus qualification before profile selection.

Provision a Python environment containing PyTorch and Transformers, then build a
candidate bundle from the acquired model:

```sh
python3 scripts/bundle_embedding.py --model /path/to/local/model --destination .private/embedding-candidate --python /path/to/python --name candidate --license Apache-2.0 --dimensions 384 --max-input-tokens 256
target/debug/know-things assets verify .private/embedding-candidate
target/debug/know-things assets import .private/embedding-candidate .private/embedding-installed
target/debug/know-things local .private/data --embedding-bundle .private/embedding-installed | cat
```

Run `reconcile` to populate the new embedding space. It activates only after all
eligible chunks are covered. Bundle hashes, tokenizer/configuration, pooling,
normalization, prefixes, and runtime versions identify the embedding space.
The supplied worker uses CPU inference with two threads, local-only model loading,
and explicit token-weighted windows for long inputs. This backend is a candidate
implementation, not a qualified model recommendation. Select either an external
research interpreter or a captured runtime as described below.

A local macOS Python environment can now be captured as a separate runtime candidate:

```sh
python3 scripts/capture_embedding_runtime.py --python /path/to/provisioned/python --binary target/release/know-things --destination .private/embedding-runtime
.private/embedding-runtime/python-runtime -c 'import torch; print(torch.__version__)'
```

The capture reads the source environment, copies its interpreter/standard library and
declared CPU embedding dependencies, relocates non-system native libraries, and runs an
isolated tiny random-weight CPU fixture before publishing. It never downloads weights
or changes the source environment. The runtime launcher supports worker scripts, `-c`,
`-m`, and stdin; user/site customization and `.pth` hooks are disabled. The captured
runtime passed relocation on the current macOS host. To include it in the model bundle,
replace `--python` with `--runtime-bundle .private/embedding-runtime` and supply the
current `--binary target/release/know-things`. Assembly verifies and copies the runtime,
uses a relative launcher, and publishes atomically into a new destination. Model-repo
Python files are omitted and symlink entries rejected. Supported tokenizers must expose
the native Tokenizers backend. Pooling v2 applies native special tokens to complete,
non-overlapping token windows and checks that no input tokens were lost. The real MiniLM candidate is installed and passes operational checks;
trained-profile retrieval/answer-quality qualification remains unfinished.

## Native Flash adapter

The structured worker and Rust adapter are implemented as a qualification baseline.
Capture a content-pinned artifact without changing the Flash source checkout:

```sh
python3 scripts/capture_flash.py --source /path/to/flash --destination .private/flash-capture
target/debug/know-things flash-probe .private/flash-capture/artifact
```

The worker uses framed JSON over separate request/event and control pipes, confirms
artifact identity, bounds output/backpressure, resets state, and stops itself under
system memory pressure. It supports tokenizer-only checks without loading weights.
The captured native artifact has passed build, handshake, health, and relocation
checks. Native health detects the Apple M5 Max Metal device. A pinned 27B Q4 candidate
has generated three cited fixture answers with networking denied by the OS; corpus
quality and operational qualification remain unfinished. See the current validation progress.
A dense profile bundle builder is provided in `scripts/bundle_flash_profile.py`;
run its `--help` for explicit asset/configuration arguments. It clones local weights
where supported and never downloads them.

To enable answers, pass both verified bundles when starting `serve`, `local`, or
`mcp-stdio` (the worker must advertise `visible_token_metrics_v1`):

```sh
target/debug/know-things local .private/data --flash-bundle .private/flash-capture/artifact --generation-profile .private/generation-profile < requests.ndjson > responses.ndjson
```

Generation startup defaults to 120,000 ms. For checkpoints requiring initial weight
conversion, add `--generation-startup-deadline-ms 600000` (range 1,000–900,000 ms).
This bounds asset verification and loading; individual answer deadlines are separate.

Answer output defaults to 1,024 tokens. Choose a profile whose context fits both the
input and output budgets; new dense bundles default to an 8,192-token context.

An answer request is, for example:

```json
{"schemaVersion":1,"requestId":"answer-1","operation":"answer","question":"What is the cache policy?","collections":["kb"],"profile":"installed-profile-name","mode":"hybrid","evidenceTokenBudget":3000,"inputTokenBudget":4096,"outputTokenBudget":1024,"deadlineMs":30000}
```

Responses carry `accepted`, `evidence_ready`, `answer_delta`, then `completed` or
`failed` events under `data`. Nonterminal status is `streaming`. Use
`{"schemaVersion":1,"requestId":"cancel-1","operation":"cancel","runId":"RUN_ID"}`
to cancel. One generation is active and one may wait; excess requests return `busy`.
Actual tokenizer counts enforce the full prompt/output budgets. Source exclusions
fence pending output before acknowledgment. References are checked mechanically;
`semanticValidation: "not_evaluated"` makes the remaining accuracy limitation explicit.
Generation initialization failure leaves retrieval available and appears in `status`.

Actual model qualification remains required. Captured artifacts are private research bundles;
source licensing and complete distribution packaging still require resolution.

## Offline installation candidates

Build a private package from locked, cached Rust dependencies and optional verified
local assets:

```sh
python3 scripts/package_installation.py --destination .private/package-candidate --flash-bundle .private/flash-capture/artifact
.private/package-candidate/bin/know-things assets import .private/package-candidate .private/installed-package
.private/installed-package/bin/know-things flash-probe
.private/installed-package/bin/know-things local .private/package-data | cat
```

Omit `--flash-bundle` for a core-only package. Optional `--embedding-bundle` and
`--generation-profile` include those assets; a generation profile requires the
exact Flash bundle it binds. Packaging builds with `--locked --offline`, captures
source/build hashes, generates the operation schema, and collects dependency license
metadata and available top-level notices. It loads no models. Installation uses the
same verified atomic no-overwrite importer; destination directories must be new.
Compare the package manifest SHA-256 with the builder's receipt after transfer, then
run `assets verify PACKAGE` to check its payload.

The launcher locates assets within its installed directory, including after relocation,
and supplies their paths for `serve`, `local`, and `mcp-stdio`. Other commands pass
through to the core. Keep data and source roots outside the installation; retain old
versions side by side and back up before a schema upgrade. `USAGE.txt` and `schema.json`
are included. Python is needed for the build script, not for the installed Rust core.
Embedding bundles made with `--python` record an external runtime requirement. Those
made with `--runtime-bundle` include the runtime and use it after relocation; the package
records its manifest identity. Packages remain private candidates: distribution
licensing, broader runtime/resource testing, and release qualification are unfinished.

On this workstation, the E5/Qwen private candidate at
`.private/relocated/know-things-e5-qwen38-003` contains the tested core and pinned
static-prefix worker/profile. Its two full corpus regressions meet the request p95
latency target, and it passes relocated offline generation and cancellation/recovery
checks. See the [local validation](docs/VALIDATION.md).
Start its launcher with:

```sh
.private/relocated/know-things-e5-qwen38-003/bin/know-things local .private/interactive-kb | cat
```

Use the registration/import workflow above for a new store.
The evaluated answer settings are `profile: "qwen38-27b-static-prefix-candidate"`,
`evidenceTokenBudget: 800`, `inputTokenBudget: 3840`, `outputTokenBudget: 1024`,
and `deadlineMs: 30000`, with strict hybrid retrieval. These are explicit experimental
settings; the API's default evidence budget remains 3000. See
[validation results and remaining limits](docs/VALIDATION.md).

## Verification

```sh
python3 scripts/measure_retrieval.py --source /path/to/docs --store .private/smoke-data --output .private/smoke.json
```

This runs lexical diagnostics, verifies every returned span, checks source hashes
before/after, and records latency. It does not measure labeled evidence recall or
answer quality. Private sources, indexes, models, and traces belong outside version
control. Lifecycle tests use disposable fixtures and never mutate the real KB.

[The evaluation workflow](docs/EVALUATION.md#answer-capture-and-source-review) also
provides `evaluate_answers.py` for private local answer/citation capture and
`score_answers.py` for separate hash-bound source review. Mechanical reference checks
do not establish factual correctness; failures remain in quality denominators.
Broader model, latency and independent answer-quality qualification remain open.

`backup` stages and syncs a consistent SQLite snapshot, then atomically publishes it
at a new `destination` file without replacing an existing path. To restore,
use `know-things restore BACKUP.sqlite NEW_DATA_DIR`. The destination must be new;
restoration checks a supported store schema and canonical integrity before publishing
it. Run `integrity` afterward. Empty SQLite files are rejected as backups. Publication
also refuses destinations created concurrently during restore or asset installation.
Restore rebuilds lexical data from validated canonical chunks in its staging directory.
For derived lexical damage in an existing store, submit:

```json
{"schemaVersion":1,"requestId":"repair","operation":"rebuild_lexical","deadlineMs":5000}
```

This preserves retained references, vectors, and exclusions; it reads no source files
and runs no models. Canonical corruption is rejected. Successful explicit repair advances
corpus generation once; failure rolls back. Restore preserves the backup's generation.
The current backup contains canonical state and embedding manifests, not model
weights or an external Python environment. Complete asset/runtime provisioning and release
installation workflows remain on the delivery plan.

## Documentation map

- [Design and research](DESIGN.md): product goals and maintained design.
- [Architecture](docs/ARCHITECTURE.md): decisions, invariants, and upgrade rules.
- [Contracts](docs/CONTRACTS.md): identity, query semantics, and failure behavior.
- [Corpus](docs/CORPUS.md): external read-only sources and private evaluation data.
- [Validation](docs/VALIDATION.md): release gates, experiments, and milestones.
- [Evaluation](docs/EVALUATION.md): revision-pinned labels, splits, and honest retrieval metrics.
