# Evidence and inference contracts

Contract v1 · 2026-10-22 · Executable baseline; qualification gates remain open

These contracts govern all front ends. `src/contracts.rs` and its derived schema are
the executable public representation. Do not maintain separate manually edited CLI,
MCP, and worker schemas.

## Identity and revisions

- `collectionId`: opaque stable identity for a configured document collection.
- `documentId`: opaque stable identity within a collection. A rename may preserve it only when the source adapter establishes identity; matching content alone does not prove a rename.
- `revisionId`: immutable identity tied to source bytes/hash and source provenance. Reindexing with a new parser can reuse raw revision bytes while producing a new processing generation.
- `chunkId`: immutable identity tied to revision, parser/chunker configuration, and exact source spans. Never use a mutable chunk ordinal as the sole public handle.
- `embeddingSpaceId`: complete embedding manifest identity, including tokenizer, prefixes, pooling, normalization, quantization, runtime-sensitive settings, and dimensions.
- `corpusGeneration`: monotonically increasing store-local commit identifier for query-visible content, eligibility, and active model-space changes.
- `evidencePackId`: hash of canonical pack content and ordered references, scope, generation, budgets, and relevant model/processing identities. The hash algorithm and canonical encoding are versioned.

IDs are opaque to consumers and scoped to a particular store. Scores and auto-increment row IDs are not evidence identities. Content hashes may deduplicate storage without merging source provenance or access eligibility.

## Common operation envelope

Each operation returns `schemaVersion`, `requestId`, a status, and either typed data or a typed error. Include operation duration and relevant generation/model identifiers. Cursors or handles are opaque, bounded, and versioned if introduced.

Error categories include invalid request, unsupported version/capability, source unavailable, evidence version unavailable, model unavailable/incompatible, deadline exceeded, canceled, busy, resource exhausted, store incompatible/corrupt, and worker failed. Stable machine codes are separate from human-readable detail.

Only retry errors explicitly marked retryable. A retry is a new request unless an operation has documented idempotency. Logs must not leak source text, prompts, or secrets through error detail.

File-descriptor exhaustion (`EMFILE`/`ENFILE`), storage capacity exhaustion, and memory
allocation errors reported by local I/O return `resource_exhausted`. This classification
does not promise recovery from an operating-system kill or a process-aborting allocation.
After capacity is restored, a new worker may be started; failed startup does not authorize
changing existing evidence or corpus generation.

## Search and evidence assembly

Inputs include query, explicit collection scope, top-K, evidence-token budget, deadline, and requested mode (`lexical` or `hybrid`). Empty scope behavior is explicit configuration, never an accidental search of every readable file. Bound query length, top-K, and budget at the boundary.

The response includes:

- Actual retrieval mode, embedding-space identity, corpus generation, degradation reasons, and coverage/truncation flags.
- Ranked evidence references with document/revision/chunk IDs, content hash, heading path, source locator, and source byte spans.
- Selected evidence text, a count measured by the declared tokenizer, and a statement of whether it is a full chunk or selected excerpt.
- Optional score components labeled lexical/dense/fusion/rerank; no implied calibrated confidence.
- Stage timings for queueing, query embedding, each search branch, fusion/reranking, context fetch, and assembly. Parallel stage durations need not sum to wall time.

Dense candidates are generated independently. Missing semantic capability returns an explicit degradation reason or an error when strict hybrid was requested. A lexical result set is not passed off as a completed hybrid search.

The current lexical query treats input as text, splits alphanumeric/underscore terms,
deduplicates them case-insensitively, and combines escaped terms with OR. Repeating a
word does not implicitly increase its ranking weight. Raw FTS operators and quoted
phrase syntax are not exposed by this baseline.

For retrieval-only callers that have not selected a generation model, report counts under a named budget tokenizer. When generating locally, revalidate the pack against the actual generator tokenizer and its total context budget.

## Context resolution

Context input carries a complete immutable evidence reference and requested scope: exact span, section, neighbors, or document. Check current eligibility before returning any retained revision. Never resolve an old chunk ID to the newest document text.

If content is retained and eligible, return the exact revision with bounded expansion and truncation metadata. If purged or excluded, return a typed unavailable/forbidden result as appropriate to the caller; do not substitute current content silently. A separate explicit search can discover current content.

Source URLs may be human navigation links. Local immutable content/hash is the evidence authority when a remote URL is mutable. All cited text must be available from the evidence pack or a still-valid pinned context reference.

## Answer operation

Inputs include question, scope, selected generation profile, evidence/input/output budgets, deadline, and queue policy. A profile is a verified model/runtime manifest plus generation configuration, not an arbitrary model filename.

After baseline generator-budget trimming, answer assembly may propose omitted adjacent
same-section candidates beside retained anchors when they add question terms missing
from the anchor's text/metadata. All baseline selected spans and their relative order
must survive. Admit the complete augmented pack only if evidence/input/total-context
budgets fit under the actual generator tokenizer; otherwise use the baseline. The
evidence budget includes rendered source labels/metadata. Candidate source bytes and
references remain exact, and search rankings are unaffected. No additional context
read or source access is implied.

Answer assembly can also append one complete <=512-byte sentence item from an omitted
flat Markdown list in an already retained document/version when it adds uncovered
question terms. It rejects lead-ins, nesting, fences and continuation chunks, retains
an exact subrange of the original chunk reference, and preserves all retained spans
and order. Full expanded/actual/input/context budget checks govern whole admission.
The initial candidate snapshot's eligibility checks and revision leases apply; no
additional source read or retained-data lifecycle is introduced.

The coordinator obtains evidence, checks total template/token budgets, and submits one worker request. The response stream carries monotonically ordered events:

1. `accepted`: request/run ID and queued/active state. This is not first-answer latency.
2. `evidence_ready`: evidence pack ID, generation, ordered source references, and actual mode.
3. `answer_delta`: user-visible answer text with sequence number.
4. `completed` or `failed`: exactly one terminal event, when the transport remains available.

Completion records finish reason, prompt/prefill token counts, total generated tokens including hidden reasoning, visible tokens, timings, model/worker identity, citations, and validation status. Truncated, canceled, and failed outputs are not scored as successful complete answers.

The model view retains labels, source paths and immediate section headings,
with ancestor headings left in the API evidence. Paired same-line inline-code delimiters
are omitted from the model view; their contents, source evidence and references stay
unchanged. Fenced/indented, escaped, unmatched and multiline code is preserved.
`tokenCount` / `evidenceTokens` count the actual compact rendering with
the generator tokenizer. Selection conservatively bounds both the expanded and compact
renderings, so shortening headers does not add passages or promise to fill the budget.
Input and total-context limits apply to the actual submitted prompt. The optional
`timingsMs.evidencePreparation` records selection and tokenization time.

Workers advertising `static_system_prefix_v1` accept `warm_prefix` after a dense
profile loads. It prepares exactly one system-role prefix, with no user/source data,
up to 256 tokens within the remaining startup deadline. The response must bind the
request ID and exact tokenizer count. Older workers retain uncached behavior. Exact
full-prompt token-prefix matching gates snapshot restore; profile/worker replacement
destroys it. Optional `cachedPrefixTokens` and `timingsMs.prefixRestore` report actual
reuse, including zero on a mismatch. Full prompt/context counts and all answer
deadlines include the original prefix; restore time is included in prefill. No
operation/store schema migration or on-disk cache is introduced.

Citation validation is explicit: valid reference IDs and source spans are mechanical checks; whether the evidence entails a claim is a semantic evaluation. Invalid references cannot receive a successful validation status. If discovered after text streamed, mark the run failed/unvalidated and report the issue; do not imply prior text was verified. Do not promise that every streamed claim has been semantically checked before display.

Source documents are untrusted input. Instructions in retrieved material do not change system policy or invoke tools. Generated output does not authorize file/process/network actions.

## Flash worker protocol

Use bounded length-prefixed UTF-8 JSON frames over inherited local pipes, with a declared protocol version, maximum frame size, request ID, and event sequence. Keep a dedicated control path for cancellation/shutdown. Define exact encoding/framing in the implementation schema; fail closed on oversized or malformed frames.

Startup handshake includes protocol compatibility, worker build identity, Flash artifact identity, supported model capabilities, loaded profile, context limit, tokenizer/template identities, and memory diagnostics. The coordinator refuses an incompatible handshake.

Operations: `load_profile`, `generate`, `cancel`, `health`, and `shutdown`. Initial implementation has one active generation. No concurrent mutations, implicit profile switching during a run, or snapshot reuse across incompatible instances.

`generate` supplies the exact rendered prompt or template inputs according to negotiated capability, its expected token count, completion limit, deadline, and generation settings. Worker and coordinator agree on tokenization; mismatches are errors. The worker reports deltas and timings without echoing the full prompt.

Cancellation control is idempotent and identifies the active request. A terminal event records actual stop/reset completion. After a crash or uncertain reset, discard the worker instance and its snapshots. A worker restart does not resume or replay an earlier partial answer automatically.

Do not serialize raw engine pointers or Flash snapshots over the protocol. Snapshot IDs, if later added, are worker-local, bounded leases with explicit incompatibility/expiration errors.

## Ingestion and source lifecycle

Jobs identify source document, observed revision/hash, processing manifest, requested operation, and latest desired source-state version. Repeated delivery is idempotent. Activation rejects superseded jobs.

Statuses distinguish discovered, queued, processing, staged, active, failed, excluded, and physically removed. Report unsupported formats, parse errors, oversize content, inaccessible paths, and model failures. Reconciliation is authoritative; file watcher events are hints.

An exclusion acknowledgment means canonical eligibility changed, affected active output was fenced/canceled, and affected caches cannot serve content. It does not mean underlying storage blocks were securely erased. See [the deletion decision](ARCHITECTURE.md#decision-4-deletion-is-an-eligibility-boundary).

## Compatibility and observability

`register` accepts an optional `policy` object. An existing name/root preserves its
policy when omitted; a different explicit policy is rejected in favor of
`set_collection_policy`. Names cannot collide with existing collection IDs, and
ambiguous legacy selectors fail rather than choosing a collection. `collection_policy`
reads the current policy. `set_collection_policy` replaces it; omitted fields default
to `excludedPaths: []`, `reconcileIntervalMs: null`, and disabled `retention`.

Exclusions are case-sensitive literal relative POSIX paths matching the path itself
or slash-delimited descendants. Paths must be normalized, at most 4096 bytes each,
with no wildcard, NUL, backslash, absolute/dot/dot-dot component, repeated slash, or
trailing slash. Limits are 128 submitted paths and 16384 combined bytes; stored rules
are sorted and deduplicated. Directory pruning happens before parsing source files.
`filteredEntries` counts pruned directory/file entries, not all descendants under a
pruned directory. Existing built-in exclusions remain in effect.

Replacing a changed policy commits affected eligibility, desired-job invalidation,
FTS removal, and one generation increment together. An identical replacement changes
nothing. Relaxing a rule requires reconciliation before affected documents become
eligible. `include` cannot override collection policy or clear explicit exclusion
while policy blocks the document. Newly excluded active answer output is fenced
before policy acknowledgment, including queued output; relaxation cannot revive it.

An optional `reconcileIntervalMs` of 1000–86400000 enables periodic scans; `null`
disables them when `watch` is false. Settings persist across restart and restore. One scanner shares the
manual reconciliation limit, reports/retries `busy` after one second, and schedules
the next attempt from completion without catch-up work. Disabling stops future
attempts; it does not cancel an already admitted scan. `status.periodicReconciliation`
reports configured count, active collection, completed scans, busy skips, durations,
last summary counts, and error codes. Startup/configuration reporting is asynchronous.
Shutdown wakes and joins the owned scanner, canceling between source/embedding units.

Optional `watch: true` enables macOS native change hints and an initial reconciliation.
The default is false. Watching also enables a recovery interval of
`min(reconcileIntervalMs or 60000, 60000)` ms, scheduled from attempt completion.
Hints coalesce per collection and share manual-ingestion admission; a busy retry keeps
its one-second backoff. They are not publication or deletion acknowledgments. The usual
source/hash/eligibility checks and answer-output fences apply when reconciliation
publishes. Failed/incomplete traversal never implies deletion of unseen documents;
an unavailable root can leave the last complete index readable until successful
reconciliation or explicit exclusion/removal.

`status.periodicReconciliation.watch` reports `backend`, `maxTargets` (512),
`pollIntervalMs` (250), `safetyScanMaxIntervalMs` (60000), `observedEvents`, and per-ID
`collections` entries with `active`, `watchedTargets`, and `lastErrorCode`. It records
no raw event paths or content. The budget covers directories and supported regular
document files across collections; traversal rejects symlinks and filters exclusions
before opening targets. Failed partial subscriptions close their handles. A limit or
native-backend failure reports an error and leaves periodic recovery enabled, rather
than silently promising native coverage. Non-macOS native watching is unsupported.
Reporting and configuration take effect asynchronously. Polling pauses during an
admitted maintenance operation; no maximum staleness is promised under contention.

Reconciliation admits one active scan per coordinator, returning retryable `busy` for
concurrent scans. Source I/O, parsing, and document embedding do not hold the store
lock. Each job binds desired source state before that work, checks supersession
between embedding units, and verifies the observed file hash before atomic publication.
Activation also requires the original persisted job; collection removal/recreation
cannot revive it through a reset desired counter. Reports distinguish `active`,
`unchanged`, `excluded`, `superseded`, `filteredEntries`, and per-source `failures`. Eligibility changes
fence affected answer output before their acknowledgment. An incomplete traversal
never implies deletion of unseen documents.

Resident NDJSON reconciliation and answer requests run alongside later requests.
Clients correlate `requestId` rather than assuming response order, and must await a
reconciliation response before requests that depend on its imported content. Queries
during replacement may use the prior complete committed revision. There is no
partially published document. Native watching and recovery scans invoke this same
reconciliation path through the opt-in policy.

Pin protocol/schema versions. Additive optional fields can be compatible; changed meaning, required fields, framing, or evidence identity require a version boundary. Unknown optional events/fields follow an explicit negotiated policy; unknown mandatory behavior fails clearly.

Preserve operation IDs across coordinator/worker traces. Record durations, finish/degradation reasons, counts, build/model manifests, and corpus generations. Source text and prompts require explicit retention settings. Tests should exercise contracts through real CLI/MCP adapters as well as the core API.

## Executable baseline — 2026-10-22

`src/contracts.rs` is the executable Rust contract for retrieval and answer operations;
`know-things schema` derives their JSON Schema. Requests flatten the operation into the
envelope: `schemaVersion`, `requestId`, `operation`, then operation-specific fields.
Responses include `status`, `durationMs`, `corpusGeneration`, and `data` or a typed `error`.
Operations are named with snake_case; search/evidence fields use camelCase.

Current `context` accepts the complete immutable `reference`, optional `scope`
(`exact`, `section`, `neighbors`, `document`), and `evidenceTokenBudget`. It returns
`text`, the requested `reference`, individually resolvable selected `evidence`, exact
budget-token counts, and truncation metadata. The target chunk receives budget priority;
selected spans are returned in source order. `backup` requires a new destination file and
never overwrites an existing backup. Diagnostics avoid embedding SQLite/source text in
error details.

SQLite capacity/allocation/oversize failures return `resource_exhausted`; lock contention
returns retryable `busy`, and corrupt/non-database inputs return `store_corrupt`.
OS storage/quota exhaustion also returns `resource_exhausted`. Diagnostics do not expose
SQL statements or source text. A failed publication never reports a successful lifecycle
acknowledgment.

Local coordinator IPC uses a 4-byte unsigned big-endian payload length followed by
UTF-8 JSON, with a 1 MiB limit. MCP uses newline-delimited JSON-RPC stdio. The optional
embedding worker uses the same bounded frame format, an identity/runtime handshake,
monotonic request IDs, a single in-flight request, and deadlines. A failed/timed-out
worker is terminated and cannot be reused or replayed silently. These embedding frames
are separate from the proposed Flash generation protocol.

Evidence-pack hashing uses SHA-256 over compact JSON with lexicographically sorted
object keys, version 1, containing scope, generation, ordered exact references/text,
actual mode, embedding-space identity, tokenizer, and evidence budget. Timings and
ranking diagnostics are excluded. Embedding manifests use the same sorted-key compact
JSON encoding for identity; numeric manifest fields are integers and strings are UTF-8.

Answer packs additionally bind the retrieval-pack ID, generation profile, and request
budgets. Their hash payload now uses version 3 and `evidenceRendering: "leaf-headings-inline-code-v1"` to distinguish the compact rendering. Earlier answer packs used
versions 1 and 2. These IDs are opaque; changing a rendering identity does not rewrite source
references or require a store/schema migration. Retrieval-pack hashing remains version 1.

Additional implemented lifecycle operations are `include` (clear exclusion, require
reconciliation), `remove_collection` (remove canonical records, preserve source files),
`prune` (`retainRevisions` per document), and the installation command `restore` into a
new directory. Pruned references return `evidence_version_unavailable`; no operation
claims secure erasure. Store schema version 2 separates active processing from raw
revision identity; version 3 adds collection policies and version 4 adds the retention
policy format. Version 5 separates shared immutable vector values from chunk mappings;
version 6 adds the optional watch policy, defaulting old records off without rewriting them.
Public operation schema remains version 1.

Exact document-text inputs may reuse an embedding only in the same complete embedding
space. This does not merge document/revision/chunk IDs, source provenance, eligibility,
or context references. Query embeddings are separate. Retained chunk mappings own
values, including history; removing/pruning the last mapping removes its value in the
same transaction. Excluded sources remain unavailable even if another source retains
the same value. A cached value is private preparation data until the new source's
ordinary desired-state and activation checks pass.

The schema-5 migration preserves existing packed bytes and corpus generation, including
distinct historical vectors for identical text in one space. Failure rolls back the
whole 4→5 step; invalid canonical vectors are rejected rather than repaired by
deduplication. Older binaries refuse the upgraded store. Restore accepts schema 1–6
backups and upgrades a copy without changing the original backup. Back up before
opening an existing store with a newer binary; no in-place downgrade is provided.

The native Flash worker now implements the framed-protocol baseline. Its handshake
identifies compiled worker/source artifacts and capabilities before a model is loaded.
`load_profile` confirms a verified profile ID/context limit; `load_tokenizer` supports
CPU-only tokenizer diagnostics. Generation events carry request ID, monotonic sequence,
token counts including reasoning, terminal reason/timings, and reset completion. The
Rust adapter rejects invalid frames/IDs/sequences/budgets and never reuses a failed
instance. Public answer streams use the same adapter through the coordinator queue.

Generation profiles bind `flashArtifactId` to the SHA-256 of the entire canonical
verified Flash bundle, including executable/library hashes. The native handshake's
compiled source/build labels remain diagnostics checked against that bundle; they
alone do not establish profile compatibility. Stored vectors in schema v2 are
L2-normalized IEEE-754 float32 values encoded little-endian, with dimensions and
embedding-space identity supplied by the stored manifest.

## Implemented answer and transport semantics

`answer` requires `question`, explicit `collections`, and the installed profile's
name or ID in `profile`. Defaults are `mode: "hybrid"`, `strictHybrid: false`,
`evidenceTokenBudget: 3000`, `inputTokenBudget: 4096`, `outputTokenBudget: 1024`,
`deadlineMs: 30000`, `queuePolicy: "wait"`, and `queueDeadlineMs: 30000`.
`queuePolicy: "reject"` declines waiting. Total deadlines start at admission; a
queue deadline can expire sooner. Busy errors report bounded queue state.

Each event has `runId`, a strictly increasing `sequence` starting at one, and `event`
inside response `data`. Nonterminal envelopes have `status: "streaming"`; terminals
have `ok` or `error`. A successful `completed` event contains `answer`: visible text,
profile/tokenizer/build identity, evidence-pack ID, exact citations, actual retrieval
mode/degradation, finish reason, counts, and timings. `referenceValidation` is mechanical;
`semanticValidation` is always `not_evaluated` in this baseline. `[S1]` labels identify
ordered evidence entries. Malformed, absent, or unknown citations fail validation,
except for the exact policy abstention. Streamed text is provisional until terminal.

An empty evidence pack produces a fixed local abstention without generation. Its
`generatedTokens` is zero and `visibleTokenCountKind` is `tokenized_policy_text`;
normal completions use generated visible/hidden metrics. Hidden reasoning and markers
consume the output limit and are never forwarded. Reaching the limit fails with
`output_truncated`, rather than reporting a complete answer. Prompt-control escaping
does not alter returned source bytes or reference spans. Evidence and full rendered
prompt are separately counted with the profile tokenizer; no engine truncation is allowed.

`cancel` takes `runId` and is idempotent for a retained run. Its acknowledgment fences
delivery immediately. A terminal failed event reports `workerStopped`, meaning that
run's execution has stopped, not that the persistent process has exited. If stop is
still uncertain one second after cancellation, it reports false. Native generation I/O
enforces that grace on partial reads/writes and terminates only its owned unresponsive
worker; it never replays the request. Preparatory work remains serialized until it
returns or reaches its total deadline; no second request uses its uncertain state. Exclusion, pruning, and collection removal use the same delivery fence
for affected references. Output already written before acknowledgment cannot be recalled.

Answer evidence IDs bind the base retrieval-pack ID, selected references/text, profile,
actual tokenizer, and budgets. Timing fields distinguish first visible production from
delivery; `durationMs` at terminal measures readiness to write, not client wire time.

CLI socket requests stream frames until a non-streaming terminal; resident NDJSON
streams lines with the same request ID. MCP returns the final core envelope as its tool
result. With a client progress token, it emits each core event under notification
`params._meta["io.know-things/answerEvent"]`; progress is strictly increasing and ends
at terminal. Active request/progress IDs must be unique. The adapters admit up to eight
active MCP tool calls. MCP `notifications/cancelled` handles active answers while a
tool runs and suppresses that canceled RPC's final response. Unknown/completed or
non-answer cancellation notifications are ignored. These behaviors follow the MCP
[progress](https://modelcontextprotocol.io/specification/2025-06-18/basic/utilities/progress)
and [cancellation](https://modelcontextprotocol.io/specification/2025-06-18/basic/utilities/cancellation)
contracts. Explicit `cancel` remains available as a tool operation with an acknowledgment.

Event frames are limited to 1 MiB; output buffers retain at most 16 events, individual
visible deltas at most 4,096 bytes, and aggregate visible text at most 128 KiB. A blocked
producer fails after 500 ms. Client socket/stdio writes have a 50 ms deadline; transport
failure cancels the run. Resident stdio output must be a pipe/file, never a terminal.
The coordinator's write callbacks must not reenter coordinator operations.

Backup creation stages and syncs a complete SQLite file before atomic no-overwrite
publication. Restore rejects unrecognized/empty SQLite files, upgrades supported older
schemas in its disposable staging directory, and preserves the original backup bytes.
Asset install/restore publication refuses a destination created concurrently, including
empty directories and symlinks. Import rejects a staged manifest that differs from the
one verified at the start. File and directory synchronization precedes publication;
failed staging stays outside the requested final path.

Installation candidates use asset bundle schema 1 with `kind: "installation"` and
entrypoint `bin/know-things`. The payload includes `bin/know-things-core`, a relative
launcher, `schema.json`, `Cargo.lock`, `dependencies.json`, available dependency notices,
and `USAGE.txt`. Optional nested bundles live under fixed `assets/embedding`,
`assets/flash`, and `assets/generation-profile` paths. Their original bundle identities
are preserved. Profile assembly rejects a mismatched Flash binding without loading
weights. The launcher adds asset options only for `serve`, `local`, and `mcp-stdio`;
`flash-probe` without a path uses the included Flash bundle. Passing duplicate asset
options fails explicitly through the core option parser.

The builder reports the outer manifest SHA-256 for trusted transfer comparison.
`assets verify/import` check payload consistency with the selected manifest; they do
not establish authenticity without a trusted expected manifest identity. Build inputs
are read-only, and output cannot be placed inside an input asset bundle. Import remains
atomic/no-overwrite. Qualification metadata explicitly remains false for release,
model profiles, and complete runtime closure; external embedding runtimes are listed.

On-disk asset manifests are bounded at 8 MiB, independently of the unchanged 1 MiB
IPC/event limits. The schema-1 manifest layout is unchanged; an older verifier with
the previous 1 MiB bound rejects larger runtime manifests explicitly.

An `embedding_runtime` bundle contains a `python-runtime` launcher, isolated bootstrap,
interpreter/standard library, selected Python distributions, and non-system native
load dependencies. Capture is macOS-only and reads its selected environment without
changing it. Non-system load paths must resolve uniquely, are rewritten relative to
the copied loader, and are checked after rewriting. Original/user site customization
and `.pth` hooks are disabled. Only verified local package paths are added; startup
rejects interpreter/prefix/search paths outside the captured root. The launcher accepts
worker scripts, `-c`, `-m`, or stdin, with bytecode writes disabled.

Publication requires a CPU-only tiny random-weight BERT forward fixture. That fixture
establishes a limited runtime/relocation check, not semantic quality or general model
support. Embedding assembly accepts mutually exclusive `--python` and `--runtime-bundle`
inputs. The latter verifies/copies a nested runtime and checks its actual version
identity before publication. A relative `embedding` launcher invokes the included
`runtime/python-runtime` with `embedding.py`; external mode retains its explicit
interpreter shebang. Assembly stages and verifies all regular inert model assets,
omits model-repo Python code, rejects symlinks/output overlap, and publishes atomically
without replacing an existing destination. Licensing and hardware/profile gates remain open.

Installation manifests record `includedEmbeddingRuntime` when the verified embedding
bundle uses the known relative launcher, supported nested runtime entrypoint, and
matching runtime identity. Merely including an unused runtime directory cannot clear
external-runtime requirements. This classification is not a semantic or universal
dependency-closure qualification.

The CPU worker's pooling identity is
`masked-mean-token-weighted-windows-v2-native-postprocessor`. It requires native
Tokenizers-backed tokenization, disables implicit truncation/padding, creates complete
non-overlapping windows, checks concatenated window IDs equal the original IDs, and
adds native special tokens before inference. Every processed window must fit
`maxInputTokens`. Masked means are weighted by original payload-token counts and L2
normalized. Successful worker results add `inputTokens` and `windows` diagnostics;
errors continue to omit exception/source text. Changed worker/runtime bytes and the
pooling tag identify a different space; vectors never mix with previous implementations.

Healthy worker destruction closes the owned request pipe, allows up to one second for
EOF cleanup, and reaps that child. At grace expiry it stops/reaps only the owned child.
Explicit stop, native-generation invalidation, and I/O/protocol failure remain immediate
and idempotent; they do not wait for healthy teardown. No request is replayed by cleanup.

An embedding bundle startup failure leaves the coordinator available with no attached
embedder. `status.embeddingError` is the typed initial error (or null), while
`configuredEmbeddingSpaceId` is null. The model-load failure leaves existing document /
FTS content, references, scope, and generation unchanged. Permissive hybrid returns explicit lexical
degradation; strict hybrid returns `model_unavailable`. The same semantics apply to
CLI and MCP. This status field is an initialization diagnostic, not an ongoing health
check. Restart with repaired/verified assets to enable embeddings. Invalid options,
store-ownership conflicts, incompatible schemas, and store failures still fail startup.


## Revision pruning

`prune` accepts `retainRevisions` (1–1000 per document), optional `maxRevisions`
(1–1000 removals per call; omitted/null means no count bound), and `deadlineMs`
(default 5000, range 1–120000). SQLite work and final publication are deadline-checked;
failure rolls back all deletion and corpus-generation changes. A successful call
advances generation once, including a pass that removes zero revisions.

Retention always counts the active revision first, followed by newest stored revisions.
Pending answer revisions are retained in addition to that count. Their leases start
when the evidence snapshot attaches under store ownership, before tokenizer budgeting,
and cover active/queued output until terminal delivery, cancellation/drop, or the
answer deadline. Leases do not override source exclusions, removals, or eligibility.
They are bounded by existing answer admission/evidence limits and do not survive
restart. Search/context references alone do not create persistent reader leases.

The result includes `revisionsRemoved`, `retainedPerDocument`, `revisionsRemaining`,
`leasedRevisionsDeferred`, `moreRemaining`, and `secureErasure: false`. Remaining
counts include leased excess history; callers should yield while readers are active
and retry a bounded pass later. Current/recent retained spans remain exact; purged
history returns `evidence_version_unavailable`. This operation does not promise
filesystem compaction or secure erasure.

Collection `policy.retention` enables scheduled pruning with required `retainRevisions`
(1–1000) and `intervalMs` (1000–86400000), optional `maxRevisions` (default 128,
range 1–1000), and `deadlineMs` (default 1000, range 1–5000). Omission/null disables
automatic retention; disabled retention is omitted from serialized policy. Settings
persist across restart and backup/restore. Schema 4 protects this new stored contract
from older binaries; migration preserves existing rows and defaults retention off.

The existing periodic worker schedules collection-scoped cleanup independently of source
scans. It shares ingestion admission and skips busy delivery/store ownership, retrying
after one second. It reloads the current policy after admission; a disable/replacement
acknowledged first cannot be followed by a pass using stale settings. Already admitted
transactions finish before the policy acknowledgment. Each pass honors leases and its
count/deadline; deferred/excess history waits for the next configured interval. Empty
automatic passes leave generation unchanged; successful removal advances it once.
No source files or model inference are accessed. Retention is an eventual revision-count
target; it does not guarantee a hard byte budget or filesystem compaction.

`status.periodicReconciliation` adds `configuredRetentionCollections`, `activeOperation`,
and a `retention` map by collection ID. Each retention entry reports `completedPasses`,
`busySkips`, `lastErrorCode`, `lastDurationMs`, and the last count-only pruning report.
The existing `collections` map continues to report reconciliation scans separately.

## Lexical-index recovery

`rebuild_lexical` accepts `deadlineMs` (default 5000, range 1–120000). It takes the store
maintenance lock, validates canonical state independently of the old FTS index, recreates
all four lexical fields from eligible active-processing chunks, and validates the new
index/database in the same transaction. Canonical corruption is a failure; repair never
rewrites raw revisions, scope, vectors, or model identities. Success returns
`rebuilt: "lexical"`, `eligibleChunks`, `corpusGeneration`, `referencesPreserved: true`,
and `sourceFilesRead: false`. It advances corpus generation once while preserving retained
reference resolution and exclusions. Failure leaves the prior index and generation intact.
Source collections and model inference are not accessed. Foreground operations serialize
with this bounded maintenance transaction. An already assembled answer retains its original
evidence generation and citations.

Restore requires a supported, consistent SQLite backup and rebuilds lexical data in its
staging directory before publication. A missing lexical table can be reconstructed from
canonical chunks. The original backup remains read-only and restore retains its recorded
generation. Corrupt/unverifiable canonical state or failed rebuild prevents publication.

Corpus generation is parsed and incremented as a checked signed 64-bit integer. Invalid
metadata fails with `store_corrupt`; exhausted generation capacity returns
`resource_exhausted` and rolls back the lifecycle transaction. It never wraps, rounds,
or silently becomes zero through SQLite numeric coercion.

Document desired-state counters also remain signed integers: exhausted capacity rejects
staging, exclusion, and inclusion before changing desired state or eligibility. It never
rounds a lifecycle counter into SQLite REAL storage or authorizes a late job that way.

## Generation initialization budget (2026-10-23)

Resident CLI modes accept `--generation-startup-deadline-ms` only with both Flash
and generation-profile bundles. The default is 120000 ms; explicit integer values
must be 1000–900000 ms. The deadline covers generation asset verification and worker
loading. Failed initialization leaves retrieval available and exposes the typed
`status.generationError`; it does not enlarge answer or queue deadlines. This is a
process option, not a change to operation schema 1 or the store schema.

On 2026-10-23 the default answer output allowance increased from 200 to 1024 tokens
after two of sixteen answerable KB tuning requests hit the smaller limit. Explicit
request limits remain supported and truncations remain failures. The selected profile
must fit the declared input plus output budgets; new dense candidate bundles default
to an 8192-token context. The 30-second default answer deadline and all hard maxima
remain unchanged. This default change requires no operation/store schema migration.

### Grouped source citations (2026-10-23)

Answers may use separate citations (`[S1][S2]`) or comma-separated groups
(`[S1, S2]`). Every member must be an ASCII positive source index within the supplied
pack. The complete group is rejected if any member is malformed or unavailable;
valid members do not conceal an invalid one. Returned `citations` still lists unique
canonical labels and their exact references in index order. Streamed/delivered text
is unchanged. This accepts a common generated format without relaxing evidence
identity, eligibility, or semantic-validation rules; no schema/store migration is needed.
