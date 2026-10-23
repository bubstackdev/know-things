# Architecture decisions

Status: starting stack accepted by the user; detailed design decisions recorded 2026-10-22.

## Ownership and scope

Know-Things indexes documentation and produces source-grounded answers locally. Source-code parsing, symbol resolution, and code relationships belong to knowcode. Preserve fenced code examples and identifiers in documents without building a code-analysis subsystem.

Initial inputs are configured local Markdown and UTF-8 text roots. README files and locally mirrored wikis are documents. Remote synchronization, PDF/OCR, and office-document support are separate adapters added when needed. File type support must be explicit; unsupported or failed files appear in ingestion status rather than disappearing silently.

Documentation sources are configured per installation; [CORPUS.md](CORPUS.md) defines read-only handling and private evaluation records. Actual-corpus quality and lifecycle correctness precede synthetic scale optimization.

Initial generation platform: macOS on Apple Silicon through Flash. Keep retrieval/storage independent of Metal so future portability does not require rewriting evidence semantics. Do not claim support for a memory tier until a model is qualified there.

## Decision 1: one Rust coordinator, embedded state

Use a resident Rust process for collection management, ingestion scheduling, retrieval, evidence assembly, generation orchestration, and diagnostics. CLI and MCP are thin adapters over the same typed operations. Do not create independent implementations behind each interface.

Use modules with narrow boundaries: source adapters, parsing/chunking, canonical store, lexical/dense retrieval, evidence assembly, inference adapter, and front ends. Begin with a small Cargo workspace organized around actual dependency boundaries; do not create a crate per module preemptively.

One coordinator owns writes to a data directory, protected by a process-lifetime ownership lock. Multiple clients connect to that coordinator. A second coordinator must not write the same store. Library-level separation allows deterministic tests without requiring a running daemon.

Reconsider only if a measured workload requires independent processes beyond the inference worker. No mandatory external database, broker, container, or remote service.

## Decision 2: SQLite is canonical; FTS5 updates are transactional

Store canonical document revisions, chunks, binary embeddings, collection membership, ingestion jobs, model manifests, active revisions, and schema state in SQLite. Use WAL on a local filesystem, a single bounded writer, short read transactions, and explicit migrations. Package a pinned SQLite build with required capabilities rather than depending on an arbitrary system SQLite.

FTS5 indexes canonical chunk text and separately weighted document title, heading path, and identifier fields. Maintain FTS rows in the same transaction as canonical visibility changes. Migration and integrity checks verify agreement. Do not rely on a background task to eventually repair normal FTS consistency.

Lexical query terms are deduplicated case-insensitively before constructing the escaped
OR query. A 2026-10-22 tuning diagnostic showed that repeated natural-language words
could disproportionately boost unrelated headings and displace supporting spans.
This query-only correction preserves stored indexes and requires no migration.

Initial dense search computes exact similarity over eligible binary vectors from the same read snapshot. Store explicit float format, dimension, and embedding-space identity; reject malformed/non-finite vectors. Never deserialize JSON vectors on the hot path. Retrieve dense candidates independently of lexical candidates.

Schema 5 (2026-10-22) separates immutable vector values from chunk ownership. The
initial multi-collection requirement exposed repeated inference/storage for identical
document inputs: two fixture documents previously required two calls and two blobs.
Reuse is keyed by exact chunk-text SHA-256 and full embedding-space identity, including
document prefix, tokenizer, pooling, normalization, runtime, and worker artifacts.
Queries retain their separate inference path. Values bind the input key, space, and
packed bytes in a versioned identity; chunk mappings retain independent provenance,
eligibility, and retained-revision ownership. Dense retrieval joins through those
mappings under its existing filtered snapshot. No ANN or external cache is added.

Parsing, text hashing, and new inference stay outside coordinator store ownership.
Each input lookup briefly reads the canonical pool; repeated chunks within one
preparation share an owned buffer. Activation publishes new values and mappings in
the same transaction as revisions, FTS, and generation. A staged cached buffer remains
valid if another document is removed before publication; only the staged document's
current desired state authorizes its new mapping. Exclusion never returns bytes from
the vector pool. Reusing a numerical value does not inherit another source's eligibility.

Pruning/removal deletes chunk mappings and transactionally removes a value only after
its last mapping disappears. Retained history and answer revision leases keep their
normal ownership; there is no independent unbounded cache or orphan cleanup queue.
Integrity verifies input hashes, packed-value identities, model/dimension/normalization,
ownership, and absence of unreferenced values. Cached data is validated before reuse.

The 4→5 migration validates legacy canonical data, streams old vectors into shared
values/mappings, drops the legacy table, and advances schema in one FULL-synchronous
SQLite transaction. It preserves every existing packed vector and reference/generation.
Distinct legacy results for the same input/space remain distinct values; no historical
rank is silently changed to force deduplication. Subsequent reuse selects the lowest
value ID for that input deterministically. Migration temporarily needs both table
representations; capacity failure rolls back to schema 4. Back up before upgrading;
older binaries refuse schema 5, and rollback uses the old binary plus its pre-upgrade
backup rather than an in-place downgrade. Restore accepts schemas 1–6 and upgrades only
its private staging copy.

This baseline intentionally avoids cross-file index publication. It may not satisfy million-chunk latency targets; the benchmark determines the next implementation. Exact search remains the correctness oracle if ANN is adopted.

Do not invent a generic pluggable database framework. Isolate dense candidate retrieval behind a small contract so its implementation can change without changing document identity or evidence responses.

## Decision 3: immutable revisions, atomic visibility

Parsing and embedding happen outside the write transaction. Persist a staged job and its source-content identity, then atomically activate the completed revision, update eligible chunks/FTS rows, and advance the corpus generation. A query reads one SQLite snapshot for both retrieval branches and evidence materialization.

Read vector data in bounded batches under that snapshot. Do not hold the transaction while generating an answer. The completed evidence pack owns the immutable content needed for generation.

Before activation, compare the job's observed source identity with the latest desired source state. Late jobs cannot resurrect deleted documents or overwrite a newer edit. Jobs are idempotent; restart reconciliation retries incomplete work and verifies sources rather than trusting file-watch delivery.

Initial reconciliation may publish per document. Expose indexing state and source revisions honestly; do not imply an entire collection is a single Git commit unless a future collection-level publication mode guarantees it.

## Decision 4: deletion is an eligibility boundary

Deleting or excluding a document first commits a tombstone/eligibility change and advances the corpus generation. New search and context operations must honor it, including pinned references and caches. Retaining old revision bytes does not authorize returning them.

For affected active answer streams, prevent further deltas from being forwarded, cancel the generation job, discard queued worker output, and then acknowledge the deletion. Already delivered text cannot be recalled. The worker may finish an in-flight GPU operation, but its output cannot be delivered after acknowledgment.

Physical garbage collection is separate and bounded. It may remove unreferenced revisions, vectors, cached prefixes, and derived indexes after readers release them. User-facing deletion status distinguishes logical exclusion from physical removal; do not claim secure erasure from ordinary file deletion.

## Decision 5: structure-aware parsing with verifiable spans

Preserve immutable raw source bytes and their hash. Parsed Markdown blocks reference half-open byte spans in that exact UTF-8 revision; lines are display metadata. If a format later needs normalization or extraction, retain the raw-to-extracted mapping and parser version.

Use heading/paragraph/list/table/fence boundaries. Chunking targets are configuration, not source identity. Count evidence with the selected generation tokenizer; embedding limits use the embedding tokenizer. Word counts are never labeled token counts.

Keep a raw identifier field and a natural-language field so filenames, dotted names, hyphens, and error codes are not destroyed by prose tokenization. The identifier fixture decides whether FTS5 needs custom tokenization or an additional exact field before considering Tantivy.

Oversized blocks are split with explicit continuation metadata where possible. Evidence assembly may select a supported span from a large chunk; it must never exceed the request budget silently or cut a code/table fragment without indicating truncation.

## Decision 6: local models are explicit versioned assets

Embeddings use a small local model initially, preferably CPU inference to preserve GPU capacity for generation. The exact model and runtime are qualified together. Flash is used for supported generation architectures; no embedding capability is assumed.

A model manifest identifies artifact hashes, tokenizer, model architecture, quantization, runtime compatibility, prefixes, pooling, normalization, context limits, chat template, special tokens, licenses, and configuration. Changing any retrieval-affecting component creates a new embedding space. Equal vector dimensions do not establish compatibility.

Stage model/embedding-space upgrades alongside the active space and switch only when eligible coverage is complete and quality gates pass. Keep lexical availability independent of embedding model availability. Fail explicitly if disk/memory cannot accommodate an upgrade; do not partially switch or fall back to cloud inference.

No downloads occur in query handlers. Explicit installation can fetch assets with progress and integrity checks; offline provisioning accepts the same verified manifests. Avoid packaging multi-gigabyte weights into the executable.

An installation candidate is itself a verified asset bundle. Its compiled core,
relative launcher, operation schema, Cargo.lock, source/build receipt, dependency
notices, and optional nested model/Flash bundles share one content manifest. Assembly
builds with locked offline dependencies, rejects changing source inputs, and publishes
through the existing no-overwrite importer. It never downloads or loads models.
The launcher supplies installed asset paths for coordinator modes and leaves client /
maintenance commands unchanged. Model/profile identities remain unchanged after copying;
generation profiles must bind the included Flash bundle. State directories remain
outside the installation. Versioned installations can coexist; store migrations still
require their documented backup/restore boundary.

Packages currently remain private candidates. Collected metadata/top-level notices
are inputs to licensing review, not redistribution clearance. An embedding bundle's
external interpreter/runtime is recorded when not bundled. Included runtimes are
verified as nested bundles and recorded by identity; classification requires the
known relative launcher rather than directory presence alone. Package
assembly does not establish model quality, runtime closure, or supported hardware.

A macOS embedding-runtime capture adapter can now copy an explicitly selected local
environment without modifying it. It includes the interpreter, standard library,
declared package closure, and non-system Mach-O load dependencies. Only copied images
have load paths rewritten and are signed ad hoc. Native dependencies must resolve
uniquely; unresolved/ambiguous paths prevent publication. The launcher uses the actual
Python.app executable where required, isolated/no-site startup, a verified local package
path, and guards against original/user import paths. An isolated CPU random-weight
fixture must pass before the bundle is published through the existing importer.
The runtime remains a host-specific private candidate. Embedding bundles can now include
it with a relative launcher; no trained weights or semantic qualification are supplied
by capture. Model/runtime assembly uses verified staged import and atomic publication.

The captured closure has 22,600 files and a 3.58 MB manifest, exceeding the original
1 MiB on-disk manifest limit. The bounded on-disk limit is now 8 MiB; IPC/event limits
remain 1 MiB. Bundle format schema 1 is unchanged. Older verifiers reject these larger
manifests explicitly. The working bundled embedding inventory is about 7.40 MB under
the current inputs, fitting without a new manifest-index subsystem.

## Decision 7: persistent Flash worker with one active request

Keep Objective-C++ and mutable Flash state behind a persistent child worker owned by the coordinator. Use a versioned framed protocol over inherited local pipes. Stdout carries only protocol frames; stderr carries logs. Do not scrape the existing human CLI.

The worker keeps one model resident and initially runs one generation at a time. The bounded FIFO queue defaults to one waiting request. A third simultaneous request receives a typed busy response with queue state; callers may set a queue deadline or decline queueing. Queue time is part of latency.

Provide separate local control and event channels so output backpressure cannot prevent cancellation. Cap frame sizes, outstanding events, and output tokens. If an event stream stops draining, abort the request after a bounded timeout. Restart a failed worker with explicit job failure; do not silently replay a partially streamed answer.

Check cancellation between safe prefill/decode units. Acknowledging coordinator cancellation stops forwarding output immediately; worker completion reports when GPU work actually stopped. Reuse a worker only after a confirmed reset or valid snapshot restore. Escalate only against this coordinator's own worker if it becomes unresponsive.

Package a pinned, licensed Flash artifact with kernels/tokenizer dependencies resolved from installed resource paths. The current sibling checkout is research input, not a release dependency. Do not modify that checkout or depend on its uncommitted files as an implicit integration step.

## Decision 8: compact evidence, bounded optional sophistication

Routine retrieval performs independent lexical/dense search, deterministic fusion, batched context reads, overlap removal, and evidence selection. Start without query expansion or reranking. Add them only after measured quality gains under a declared latency/resource budget.

Answer evidence selection can supplement budget-selected passages with omitted
immediate same-section context from the existing top-20 candidate pool. The
2026-10-23 E5/900 regression exposed a concrete omission: an adjacent policy paragraph
ranked tenth was dropped while unrelated passages from that document remained.
First compute the existing budget-trimmed selection. For each retained document's
best-ranked passage, an omitted immediate preceding/following candidate may be proposed
alongside it only when it contributes a literal question term absent from the anchor's
text, source locator and heading path. Terms use lowercased alphanumeric/underscore
words and exclude common English function words. The adjacent span must share collection, document, revision,
content hash, source locator and heading path. Standalone headings are not promoted.
Proposed context stays in source order; expansion is not recursive. Every originally
selected span and its relative order is preserved. The entire augmented pack must fit
the actual generator's evidence, input and total-context budgets without further
trimming; otherwise keep the original selection. No selected source is sacrificed to
admit an optional neighbor.

A lower-budget latency trial exposed a second cutoff gap: a long overview chunk
contained one necessary log-search bullet but exceeded the ceiling as a whole. After
the existing neighbor proposal, assembly may append one exact, complete top-level
Markdown sentence item from the existing candidate pool, only within an already
retained document/source/collection/revision/hash. It must add a literal question term
missing from all retained text and metadata. Choose greatest new-term coverage, then
original candidate rank and source order. The item is at most 512 bytes and ends in a
period; only flat-list-only, full, non-continuation chunks qualify. Lead-ins, nested
items, fenced code and incomplete lines are rejected. Its original byte range and
parent chunk identity resolve through normal context lookup. Admit it whole only when
both evidence renderings and full input/context budgets fit, preserving every retained
span and order. No recursive expansion, source read, summary, model or schema is added.

This is bounded context ordering, with no new model, index, source read or generated
search query. Search rankings/fusion scores and stored chunks remain unchanged. Every
selected source retains its exact bytes/reference; eligibility checks and the initial
revision leases already cover every candidate. Blind adjacency displaced labeled
evidence; a later term-gated reordering fixed the known omission but regressed two
answers in full generation. Both prototypes were rejected. The conservative rescue
policy passes the [local validation](VALIDATION.md),
including the previously incomplete answer, without displacing any selected span.
This is post-selection evidence, not fresh held-out qualification. No store/asset
migration is required; prior binaries can read the same
store and reproduce their earlier selection policy. New evidence ordering changes
the existing content-derived evidence-pack identity.

Score magnitude is not confidence. Return ranking scores as implementation diagnostics. Abstention uses measured evidence adequacy and answer policy; it cannot rely on an uncalibrated cosine/RRF threshold presented as probability.

Flash generation receives a token-budgeted evidence pack and exact model template. Count hidden reasoning in output budgets and latency. Reserve space for instructions, question/history, evidence, and completion before submitting a job. Reject an impossible budget rather than relying on engine truncation.

Answer assembly renders source headers as `[S1] path | immediate section`.
The API evidence retains the complete heading path, source bytes and immutable reference.
The model view omits ancestor heading repetition and paired same-line inline-code
backtick delimiters, preserving their literal contents. It leaves fenced/indented,
escaped, unmatched and multiline code unchanged. Source-role escaping still applies
to the final rendered view. Grounding instructions retain source-only answers,
untrusted-input handling, per-claim citations, conditions/exceptions/scope, conflicts,
the exact insufficient-evidence policy, and concise complete answers. API semantic
validation remains explicitly unevaluated by the runtime. The selected rendering
passes the repeated 53-question agent source-reviewed regression linked below.

Selection charges the larger of expanded and compact native-token counts. Smaller
formatting alone does not relax the selection bound; optional context and item
proposals still require question coverage and whole-proposal budget checks. The reported count is the actual compact evidence count; full prompt/context
checks also use the real compact rendering. This conservative bound may leave some
budget unused. Answer evidence identity version 3 binds `leaf-headings-inline-code-v1`; immutable
source references, store schema and embedding space do not change. An additive
`evidencePreparation` timing separates selection/tokenization from GPU prefill.
The explicit “Otherwise” branch keeps answering and exact refusal mutually exclusive.
Prefixes already over the expanded evidence budget skip compact/full-prompt counting;
retained packs still receive every actual budget check. The
[local validation](VALIDATION.md)
passes all 47 answerable and all 6 unknown questions in each of two fresh-worker runs,
with p95 first-visible output of 1.874 and 1.875 seconds at the explicit 800-token
settings. Every baseline800-token selected span and order survives; five packs gain
one exact item, closing the log-search omission without displacing retained context.
This establishes observed regression behavior on this host, not a general hardware tier.

The selected Flash artifact derives from the immutable Flash-006 source snapshot,
changing only its dense prefill batch from 512 to 1024 and adding the owned worker's
static-prefix capability. The sibling checkout and original captures remain read-only.
Captured source hashes, the header patch, worker build and generation-profile binding
are retained. The unmodified newer-Flash trial was not selected. The700-token trial
reversed a versioning condition; the first 800-token trial dropped necessary log-search
evidence. Both are retained as rejected experiments.

A worker advertising `static_system_prefix_v1` warms one static system-role prefix
at model startup and retains its instance-bound dense snapshot in memory (maximum 256
tokens). The selected instructions use 156 tokens. It contains instructions and empty
user-role framing only; source bytes, questions, answers and revisions never enter it.
Exact full-prompt token-prefix matching gates restore; a mismatch uses normal prefill.
Only the current model instance owns the snapshot. Cancellation/invalidation/profile
replacement destroys it; recovery rebuilds it within existing startup/answer deadlines.
Restore remains inside prefill and first-visible latency. The final probe measured 450 ms
of startup warming and about 180 MB of observed resident growth; that RSS delta includes
driver/allocation variation. Request restore p95 was4.31/3.40 ms in the two full runs.
Older workers remain compatible and uncached.

Document-prefix snapshots remain a later measured optimization. They would require
exact prefix/model/worker keys, bounded bytes and source lifecycle invalidation. The
current bounded cache contains the static instruction prefix only.

## Decision 9: offline behavior and local privacy

After assets are provisioned and documents imported, ingestion and queries require no network. Source sync is an explicit operation with visible credentials and provenance. Offline data can be stale; never imply remote revocation has been checked while disconnected.

Default roots exclude symlink traversal outside the configured collection, VCS internals, dependency/build directories, and user-configured sensitive paths. Reconciliation checks canonical paths and permissions again. Credentials live in an OS credential facility or explicit external configuration, never embedded into evidence or traces.

Local IPC and data directories use user-restricted permissions. MCP stdio is the first transport; no network listener is required. Any future local HTTP listener needs authentication and origin/host validation rather than trusting loopback alone.

Diagnostics default to timings, IDs, counts, and error codes. Prompt/document text retention is explicit and bounded. Backups use a consistent SQLite backup mechanism and an asset manifest, not an arbitrary copy of an open WAL database. Derived indexes are rebuildable; canonical source revisions are not disposable cache entries.

## Decision 10: evolution requires evidence and recovery

Keep migrations ordered and transactional where supported. Check schema/contract compatibility at open; an older binary refuses a newer incompatible store. Destructive migrations require a verified backup and a tested restore path. Do not silently downgrade schema or rebuild over the only copy of canonical revisions.

Introduce ANN only if exact search misses the qualified latency/resource target. A proposed ANN backend must support collection-aware eligibility, measured recall, updates/deletes, durable replay, and a generation publication protocol. SQLite remains authoritative. Stale candidates are rejected and retrieval must continue/refill to honor limits, or report incomplete coverage explicitly.

A sidecar index needs a durable operation log, replay checkpoints, atomic manifest publication, and tests for crashes at each boundary. Validate that entire lifecycle before shipping it. This is the condition for accepting the additional complexity, not work required for the exact-search baseline.

Do not claim full-document version retention forever. Configure retention budgets, reader leases, and explicit version-unavailable responses. Store enough manifests to reproduce evaluation runs and explain changes without creating an unbounded archive.

## Implementation invariants

- A returned evidence span resolves to exactly the recorded revision bytes and hash.
- One answer's evidence is assembled from one committed corpus snapshot.
- Excluded/deleted sources cannot be returned after exclusion acknowledgment.
- Independent semantic retrieval remains available at every supported corpus size, or the response explicitly reports degraded coverage.
- Embedding spaces never mix, even when dimensions match.
- CLI/MCP share operation semantics, errors, cancellation, and source eligibility.
- No cloud fallback, implicit query-time downloads, or mandatory remote telemetry.
- No action interferes with another terminal session or its jobs.

Qualification tasks and reconsideration triggers are owned by [VALIDATION.md](VALIDATION.md). Evidence and inference wire semantics are owned by [CONTRACTS.md](CONTRACTS.md).

## Implementation notes — 2026-10-22

The initial Rust package implements the retrieval foundation in one crate, with
separate contract, parsing, storage, embedding, asset, process, and transport modules.
SQLite is bundled through pinned rusqlite/libsqlite3-sys dependencies. The coordinator
uses a process-lifetime file lock, bounded framed Unix IPC, and shared typed operations.
A resident NDJSON/stdin mode and direct MCP mode provide the same operations when
socket binding is restricted. A dedicated actor serializes generation while retrieval
and lifecycle operations use coordinator store locks. Reconciliation scans, reads,
parses, and embeds outside that lock; registering desired state and publishing a
document use separate short ownership boundaries. One reconciliation runs at a time;
another returns retryable `busy`. The local stdio reader remains available throughout.
The shared embedding runtime gives waiting queries priority between background units
and checks each waiter's deadline. An in-flight inference unit is not preempted.
Hybrid query inference still holds the store lock; broad concurrency and resource
performance remain unqualified.

Retrieval-only evidence budgeting initially uses `utf8-byte-v1`, counting each UTF-8
byte as one budget token. This is an explicit conservative tokenizer, never a model
token count. Local generation must substitute/revalidate with its actual tokenizer.
The Markdown parser preserves source bytes and block boundaries, and flags oversized
block continuations. Exact and bounded section/neighbor/document context resolution are implemented.
Every selected expansion span retains its own immutable reference. Schema version 2
records active processing identity separately from raw revision identity, preventing
old/new parser output from becoming jointly eligible for the same raw revision.
The parser's current processing identity is
`markdown-blocks-v3-max4096bytes-heading4096`. Document titles are limited to 1,024
UTF-8 bytes and complete heading paths to 4,096 bytes before chunk fanout. Oversized
metadata produces an explicit parse error. Reconciliation atomically replaces prior
processing output while retained immutable references continue to resolve.

An optional verified embedding subprocess can load explicitly provisioned local assets.
The supplied CPU Python backend is a qualification candidate with two inference threads,
masked-mean pooling, explicit token-weighted windows, and L2 normalization. Its runtime
versions and complete asset hashes are part of the embedding space. It neither downloads
assets nor executes model repository code. A captured runtime can be shipped with the
worker/model bundle; actual trained-model qualification remains unfinished. This does
not establish the final embedding runtime or model choice.

The pinned Transformers runtime removed the high-level `prepare_for_model` method.
Pooling v2 therefore uses native Tokenizers encodings and post-processing, disabling
implicit padding/truncation and verifying exact payload-token coverage across windows.
Special-token counts bound each window; attention and optional type masks come from
the native encoding. Slow/non-native tokenizers are unsupported rather than silently
decoded and retokenized. The changed pooling tag and worker artifact create a new
embedding space; existing bundles retain their original pinned implementations.

The native Flash adapter now builds from a verified capture of source, kernels, and
tokenizer dependencies, with installed relative library/resource paths. Request/event
pipes and a separate control pipe use bounded frames; the Rust adapter enforces
deadlines, request/event identity, context/output budgets, and confirmed reset. Failed
instances are discarded without replay. Native memory-pressure events terminate only
that worker. Model/profile qualification remains unfinished; worker protocol fixtures
are not evidence of answer quality.

Healthy process teardown closes its owned request pipe and waits at most one second
for EOF cleanup, polling/reaping only that child. A child that does not exit is stopped
and reaped. Explicit stop, I/O/protocol failure, cancellation, and native-generation
invalidation bypass this healthy grace. This addresses the observed Python semaphore
cleanup warning without extending failure/cancellation grace paths or touching other
sessions' processes. Captured-runtime shutdown checks remain scoped to that fixture/host.

An embedding startup failure is now recorded alongside generation startup diagnostics
instead of preventing the coordinator from serving lexical requests. The failed model
is not attached, so strict hybrid fails and permissive hybrid declares lexical
degradation. Status exposes the initial `embeddingError`; request-time failures remain
request errors/degradation, not a continuous health probe. Recovery after repairing or
replacing assets requires explicit coordinator restart. Store/argument failures remain
fatal, and a model-load error never authorizes a cloud inference fallback.

The answer actor admits one active request and one FIFO waiter. Canceled or expired
waiters are removed immediately. All time spent waiting, tokenizing, retrieving, and
recovering a failed worker consumes the total deadline. A failed worker is quarantined;
the next request may create a fresh verified instance of the same profile. Partial
answers are never replayed. At most 64 resident delivery handles and 16 pending events
per run are retained; a producer blocked for 500 ms cancels its run. Queue expiry is
checked while streaming, independently of active generation. Native generation I/O
allows one second for cooperative cancellation, then stops only its own unresponsive
worker, even if the total deadline is later.

A shared delivery gate covers eligibility mutations, cancellation acknowledgments,
and actual client writes. Evidence references attach to the run under the retrieval
store lock before tokenizer budgeting starts. Exclusion therefore fences both active
generation and completed-but-undelivered output, including when a source is quickly
re-included. Edits preserve eligible pinned old revisions. Transport sinks must only
write; they must not reenter the coordinator while holding this gate. Pipe/socket
writes are bounded to 50 ms and frames to 1 MiB; visible answers are capped at 128 KiB.
Resident stdio requires a pipe or file and does not change terminal state.

Prompt assembly uses the selected generation tokenizer and reserves completion space.
Escaping control delimiters changes prompt representation only; immutable evidence
remains byte-exact. The native worker suppresses reasoning delimiters/text and reports
visible and hidden token counts. This capability requires single-token reasoning
delimiters; CPU filter fixtures and tokenizer probes do not establish real-model
behavior. Citation checks establish reference validity only, never entailment.

Offline operational primitives now include atomic restore into a new directory,
explicit revision pruning, re-inclusion requiring reconciliation, and collection
removal. Integrity diagnostics verify SQLite/foreign keys, FTS visibility/content,
raw revision hashes, chunk identities/spans, embedding manifests, and vector shape /
normalization. Automatic retention is opt-in through persisted collection policies.

Pruning now accepts a removal-count bound and enforces a SQLite progress deadline.
It deletes revisions, chunks, vectors, and derived postings transactionally, preserving
the active revision and the configured newest-history count. The coordinator leases
revisions attached to pending answers under the same store ownership used for evidence
assembly. Leases cover tokenizer budgeting, streaming, and completed output awaiting
delivery. They end at terminal delivery, cancellation/drop, or the request deadline,
and are bounded by existing run/evidence admission limits. No leases persist across
restart. Expired or excluded evidence is never revived by a lease. Client-held search
references do not establish a lease and may become version-unavailable after pruning.
Manual pruning limits require no schema migration; work limits are optional request fields.

Backup publication now stages a synced SQLite snapshot and hard-links it atomically to
a previously absent destination. Restore accepts recognized schema versions, verifies
canonical integrity, and publishes with the OS exclusive-rename primitive. Asset import
verifies that the staged bundle still equals the initially selected manifest, syncs
nested directories, then uses the same no-overwrite directory boundary. Concurrent
existing paths are preserved. Those publication changes add no store-schema migration. SQLite
capacity failures roll back publication; capacity/lock/corruption errors are distinct.

Collection source policies and periodic scans were added on 2026-10-22. Store schema 3
adds one policy row per collection; migration from schema 1/2 defaults to no additional
exclusions and no timer, preserving revisions, vectors, and corpus generation. Older
binaries refuse schema 3. Backup/restore includes policies and upgrades in staging.
Literal normalized relative path rules prune directory traversal and reject files
before reading or parsing. A current-policy check also gates staging and activation.
Policy replacement atomically hides documents whose admission changed, invalidates
their desired jobs, updates FTS, and advances generation once. Relaxation requires
reconciliation and preserves explicit exclusion. Answer delivery uses the same
eligibility fence before acknowledging policy changes. Reads admitted before a policy
change may finish privately; they cannot publish or deliver newly excluded content.

Periodic reconciliation is opt-in and persisted per collection. One owned thread
waits on a condition variable, sharing the manual ingestion mutex and implementation.
It retries a busy attempt after one second and schedules the next scan from completion,
without accumulating missed intervals. Configuration changes wake it. Shutdown signals
and joins only this owned thread, checking cancellation between files and embedding
units; an already running local I/O/inference unit may finish first. Status exposes
counts, timing, and error codes without source text. This initial scheduling step added
no filesystem watcher; the schema-6 extension below adds native hints.

Store schema 4 extends the persisted policy contract with optional retention settings.
Its version-only migration preserves existing policy JSON and all canonical state;
retention defaults off. Earlier binaries refuse the new store before interpreting its
policy format. The same periodic worker schedules independent source-scan and retention
tasks. Retention does not read sources or invoke models, is scoped to one collection,
and reloads policy after acquiring admission. It shares ingestion admission and skips
busy delivery/store ownership, retrying after one second. Each automatic pass has a
configured revision limit and at most a five-second SQLite deadline. Missed intervals
do not accumulate. Leased history is deferred to later passes; empty automatic passes
do not advance generation. Shutdown joins this worker, allowing a running bounded
transaction to finish. Retention counts are eventual history targets, not hard disk caps.

Native source watching was added on 2026-10-23. Schema 6 versions a policy with optional
`watch` (default false); its version-only migration leaves old policy JSON, references,
vectors, and generation untouched. Older binaries refuse the new schema before reading
the policy. Restore upgrades a private copy of schemas 1–6. No cloud service or source
write is introduced.

A local SDK-based FSEvents probe created a stream but could not start it on this host;
the cause was not diagnosed. A kernel vnode probe delivered an actual owned-file write
event, so the implemented macOS backend uses `kqueue` rather than retaining an unverified
FSEvents path. One kernel queue and at most 512 target descriptors across configured
collections bound native resources. Targets cover allowed directories and supported
regular documents. Exclusions prune traversal; descriptor-relative opens reject
symlinks, special files, and changed parent paths. Traversal keeps at most one walkdir
directory handle open, plus bounded transient component descriptors. Queue/target
descriptors are owned, close on exec, and close on refresh, disable, failure, or shutdown.

The existing maintenance thread polls hints every 250 ms while waiting between work
units; no additional watcher thread/process exists. Events carry opaque registration
tokens and coalesce into collection IDs without retaining event paths or source text.
Only currently owned registrations participate, avoiding descriptor-reuse confusion.
Changing a tree refreshes its subscriptions before the next reconciliation. New and
restored subscriptions request an initial scan to recover changes made while offline.
The same source checks, desired jobs, embedding priority, publication transaction, and
answer delivery fence remain authoritative. Notifications never directly mutate
canonical eligibility or serve content.

Watching enables a completion-relative recovery scan interval no greater than 60 s,
even if `reconcileIntervalMs` is null or longer; a shorter explicit interval wins.
Missed/coalesced notifications, atomic replacement, and new subdirectories therefore
retain an independent reconciliation path. A target-budget/backend/root failure closes
the partial subscription, reports its code, retries registration after one second,
and keeps recovery scans enabled. Non-macOS builds report unsupported native capability
and use that recovery path. This does not establish native watching on those systems.
Long scans or retention transactions may delay polling/recovery; the interval is not
a maximum staleness guarantee under contention. Disabling stops future tasks and closes
subscriptions after the current admitted unit finishes. Shutdown serializes its stop
predicate with the condition-variable wait, wakes and joins only the owned worker,
and closes its native handles. No other session or process is controlled.


Lexical recovery validates canonical tables independently of FTS shadow tables, including
collection/document identities, eligibility flags, active-revision ownership, immutable
source spans, model manifests, and stored vectors. Rebuild recreates FTS from eligible
active-processing chunks in one transaction, then checks the full database before commit.
Failed validation, expiration, or publication rolls back. Retained references and source
eligibility are unchanged; explicit repair advances corpus generation. Restore rebuilds
in its staging directory while retaining backup generation. No embedding space is silently
regenerated. Canonical state that cannot be validated requires verified recovery data.
Checked generation arithmetic prevents exhausted/corrupt counters from publishing partial
lifecycle changes or producing false snapshot identities. No store-schema migration is needed.

## Real-model startup costs (2026-10-23)

Large generation bundles retain complete SHA-256 verification before loading. Enable
sha2's pinned assembly backend on aarch64, with runtime hardware detection and its
software fallback, to avoid the default software hashing bottleneck. Content hashes,
asset identities, canonicalization, and database schemas are unchanged. A same-shard
diagnostic is recorded in [local validation](VALIDATION.md);
its ordered shared-host samples are not general performance qualification.

The 27B bf16 checkpoint's on-load conversion to Q4 exceeded the previous 120-second
startup budget after real Metal loading began. Resident modes now accept a separately
bounded generation startup setting (default 120 seconds, maximum 900 seconds). Asset
verification and initial loading share that budget. Request deadlines and the native
memory-pressure guard remain independent and unchanged. A fixture delayed at profile
load verifies both timeout with usable retrieval and success with a sufficient budget.

The pinned native dense loader currently requires a separate `lm_head` tensor.
A real Qwen3.5-4B checkpoint with tied output embeddings loaded its trunk but then
failed profile initialization. The profile builder now rejects tied-head checkpoints
before copying assets or starting a worker. Tied-head support requires a separately
pinned, validated loader change; no synthesized output tensors or sibling Flash edits
are introduced for this operational diagnostic. Untied 9B remains a separate candidate;
it does not resolve the required 27B/MoE selection gate.

The successful BF16-to-Q4 candidate takes longer than an ordinary 30-second answer
deadline to initialize, so worker recreation under that request deadline is not yet
qualified. The startup override changes only first initialization; it does not silently
extend recovery or answer timeouts. Flash's existing prepared group-128 affine Q4
checkpoint is a separate candidate for bounded startup/recovery. Capture its bytes,
source revision/quantizer provenance, and original model license into a new verified
bundle; never load it implicitly from the mutable sibling build directory. Model files
are hashed before/after capture and compared with captured bytes. Its changed quantizer
requires separate output/quality tests; previous BF16 on-load Q4 results do not transfer.

## Source-audited evaluation and prompt scope (2026-10-23)

The user authorized direct agent validation for the small documentation corpus; a
mandatory human-only gate is removed. Agent-reviewed labels/reviews retain distinct
provenance, source-audit hashes, exact supplied-evidence quotes, citation-occurrence
accounting, and private reasoning. Historical human/fixture formats remain supported.
This does not change runtime semantic validation or introduce cloud inference.

A tuning answer invented a universal requirement from a statement scoped to one
versioning scheme. The generation instructions now explicitly preserve conditions,
exceptions, and scope and address only the question asked. The immutable prior core
and captured answer remain available. A 1000-token evidence-budget candidate is tested
on tuning queries because the 3000-token baseline fails latency diagnostics; no ANN,
reranker, cache, or held-out-driven tuning is introduced. No store/API migration is needed.

A real tuning response cited `[S1, S2]`; the previous single-label parser rejected a
correct reference group. Source-citation parsing now accepts comma-separated groups,
validates all members before success, and returns the same unique exact-reference list.
Independent capture/review validation implements the same documented grammar and tracks
its helper hash. Regression coverage includes real coordinator completion/context
resolution, duplicate-member deduplication, mixed valid/invalid groups, missing members,
and unsupported malformed labels. No source eligibility check is weakened.
