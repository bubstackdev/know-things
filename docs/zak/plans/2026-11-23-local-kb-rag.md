# Rust implementation plan for local KB retrieval

The starting point is the [local RAG design](../specs/2026-11-23-local-kb-rag-design.md) and an unborn `main` in `know-things`. Build and verify the local lexical candidate first. The KB checkout is read-only input. The comparison with CodeGraph is a release gate.

| Order | Files and behavior | Verification |
| --- | --- | --- |
| 1 | `src/source.rs` exports one local Git commit into a bounded snapshot and supports managed host sync. `src/markdown.rs` emits source ranges. `src/index.rs` publishes one revision transactionally, uses FTS5, and provides pinned citations and range reads. | `cargo test --locked` exercises the CLI over synthetic Git commits and SQLite, including dirty files, edits, deletions, and failed refreshes. |
| 2 | `src/wire.rs` and `src/daemon.rs` provide bounded local RPC for `search`, `read`, and `status`. `src/mcp.rs` exposes those tools through the official Rust SDK. | Start a daemon in a temporary directory and call it over MCP stdio. Verify shared index lifetime, structured results, and index refresh. |
| 3 | `Dockerfile`, `compose.yaml`, and `README.md` package the arm64 daemon and its operational commands. | Validate Compose, build and run on Apple Silicon, and exercise the CLI and stdio adapter through Docker. |
| 4 | Run the evaluation harness over frozen, labeled KB questions. Compare production CodeGraph MCP behavior and corpus-matched resource measurements. | Record accuracy, latency, throughput, memory, CPU, disk, freshness, and recovery against the gates in the design. Test dense retrieval if the lexical candidate misses meaningful evidence. |

Local RPC and SQLite share the same contracts for no match, unavailable index, stale revision, and bounded excerpts. Deliver the lexical candidate with its test evidence. Release qualification requires access to a corpus-matched CodeGraph baseline and a held-out set reviewed by developers.

## Implementation and verification status

- Steps 1 and 2 are implemented in the workspace. Three native integration tests passed. They cover commit-only export, index deletion, failed managed sync, two concurrent MCP sessions, stale revision reads, malformed snapshot recovery, and daemon restart. Formatting and Clippy with warnings denied passed.
- Step 3 passed in an isolated Compose project on Docker Desktop for Apple Silicon. The Linux arm64 image indexed 33 files into 378 passages, completed MCP initialization, tool listing and search through `docker exec -i`, rebuilt into a separate database, and picked up a new commit through the read-only bind mount. The container ran as UID 65532 with networking disabled and a read-only root filesystem. The temporary project and volume were removed after the check.
- Step 4 has a local evaluator and an initial smoke result at the pinned benchmark revision. The three answerable cases had sufficient evidence at ranks 1, 1, and 2. The unsupported case returned no hits. Over 200 warm serial daemon calls, p50 was 0.205 ms, p95 was 1.292 ms, and p99 was 1.409 ms. Docker reported 4.207 MiB of cgroup memory after the run, and `/data` occupied about 1 MiB including SQLite sidecar files. These measurements cover the daemon on this Mac. A held-out comparison and like-for-like CodeGraph resource measurements are still pending.
- Managed sync was exercised against a local Git fixture and the macOS LaunchAgent template passed plist validation. The private GitLab fetch, installed LaunchAgent, and hosted Codex and Claude sessions remain to be checked during rollout.

## Retrieval evaluation follow-up

A 12-question tuning set at the same pinned commit lives in `.local/benchmark-2026-11-23/seed.jsonl`. The Docker run found sufficient evidence at rank one for 5 of 10 answerable questions and within five hits for 9 of 10. It returned unrelated evidence for both questions whose answers were absent from the KB. The missed answer needed two passages about log correlation. Search returned the service identifier convention and omitted the command for following the request. The run's p95 was 1.420 ms over 240 warm serial daemon calls.

Use these cases to tune abstention and retrieval of multiple supporting passages. Build and freeze a separate developer-labeled holdout before changing ranking. The live CodeGraph search tool filters by source kind and offers no corpus revision parameter. A scoped CodeGraph index at the same commit is required for the controlled comparison.

The first-pass holdout is in the ignored `.local/benchmark-2026-11-23/history-holdout/` directory. It has 104 questions based on Shortcut stories, GitLab merge requests and commits, ELK searches, and Sentry issues. The labels identify sufficient evidence in 16 files for 86 questions and record 18 questions as outside the pinned KB. The separate provenance file retains source references and commit-pinned evidence links. The manifest records both file hashes. Direct PagerDuty access required a credential, so PagerDuty cases came from the associated Shortcut stories.

Validation resolved every evidence span against the pinned Git commit and rejected duplicate questions and exact evidence spans repeated from the tuning seed. The holdout remains unscored. Developer review of label sufficiency and the matched CodeGraph run remain pending.

## CodeGraph comparison setup

The local CodeGraph checkout's `origin/main` at `8fe170b12cbb030ca2ad9fdd5afb3db33d700f5f` provides a direct ingestion path for a Markdown file with an explicit Git revision. Use that path with the same 33 Git blobs in an isolated CodeGraph database and its configured ONNX embedding runtime. The standard sync indexes whichever commit its checkout resolves. Keep that sync out of the benchmark. Check the build revision and corpus inventory before scoring.

CodeGraph search returns the source path, revision, and the excerpt shown to the caller. Its response does not carry source line ranges. Align excerpts to the pinned Markdown and send ambiguous matches to review. Score the returned excerpt by itself on the first pass. Count a context fetch as a separate call under the shared evidence budget. Measure the CodeGraph API, embedding runtime, and database together for equivalent resource accounting.

### Matched run results

The run in `.local/matched-benchmark-2026-11-23/report.json` verified the same 33 source blobs at the pinned benchmark revision. The CodeGraph harness built the stock retrieval pipeline at `8fe170b12cbb030ca2ad9fdd5afb3db33d700f5f` with its HTTP embedding client, the Nomic v1.5 ONNX service, and MariaDB. The runner verified keyword and dense queries before scoring. Both stacks shared the same single-vCPU affinity during search timing on Apple Silicon. Each system handled 104 labeled questions and 208 warm timing calls. Both used a 12 KiB search evidence budget and 4 KiB for one top-hit context call. The shared scorer checked complete labeled source lines in the text actually returned.

| Metric | Know Things | CodeGraph |
| --- | ---: | ---: |
| Sufficient evidence in first hit | 55 of 86 | 21 of 86 |
| Sufficient evidence in first five hits | 78 of 86 | 36 of 86 |
| Sufficient evidence after one context call | 81 of 86 | 64 of 86 |
| Outside-KB questions that still returned hits | 18 of 18 | 18 of 18 |
| Warm Docker round-trip p50 | 1.894 ms | 350.818 ms |
| Warm Docker round-trip p95 | 3.233 ms | 696.280 ms |
| Warm Docker round-trip p99 | 4.490 ms | 865.816 ms |
| Container CPU time per warm call | 1.44 ms | 356.65 ms |
| Warm container memory after the run | 7.49 MiB | 1,271.75 MiB |
| Full database directory plus model assets | 1.00 MiB | 694.57 MiB |

CodeGraph's storage total consists of 522.86 MiB for the Nomic model and tokenizer and 171.71 MiB for the entire MariaDB data directory. A separate restore of the 33-document, 355-chunk checkpoint measured 23.80 MiB in the RAG schema files, including 9.62 MiB in the full-text index files. The restored MariaDB data directory grew by 22.81 MiB from the empty schema. The restore measurements are saved in `.local/matched-benchmark-2026-11-23/index-storage-check.txt`.

At five hits, Know Things covered 44 answerable cases CodeGraph missed. CodeGraph covered two that Know Things missed. Both covered 34 and missed six. The local harness uses the production CodeGraph retrieval and embedding code with a small stdio wrapper. Resource counts include the CodeGraph runner, embedding runtime, and database. Docker VM overhead and installed runtime image sizes remain to be measured. The labels are first-pass. Answer generation, concurrent throughput, cold startup, growth, and comparative freshness and recovery remain open release checks. The benchmark stopped and removed its temporary containers, volumes, and network and retained a private CodeGraph index checkpoint.

## Know Ops telemetry verification

The Know Things daemon now publishes the Know Ops v1 report in its private tmpfs. The host relay validates each report and publishes it atomically to the per-user inbox. Native tests passed for terminal counters, discarded status calls, a degraded index, and counter reset on restart. An isolated arm64 Docker run passed for ready and unavailable reports, inbox permissions, and preservation of the last report when the container stopped. A blocked Docker read returned after five seconds and left the report intact. The LaunchAgent passed plist validation with a 15-second interval and `RunAtLoad` enabled. This run used a temporary inbox and did not exercise the Know Ops menu.

## Search abstention

The search evidence check runs after ranking and uses the passages within the requested result limit and response budget. It requires body overlap for longer queries, checks requested properties in short recognizable question phrases, and verifies explicit identifiers and ticket references. A rejected result returns `no_match` with reason `insufficient_evidence`. CLI and MCP share the behavior. A completed no-match request counts as a successful operation in Know Ops.

The rule was selected on 44 development questions, including 32 questions added as answerable and unsupported pairs on similar topics. Both builds retrieved sufficient evidence for 22 of 26 answerable development questions. Unsupported questions with hits fell from 18 of 18 to 4 of 18. The development cases and scores are in `.local/retrieval-calibration-2026-11-23/`.

With the rule fixed, the same scorer reran the 104-question holdout through MCP stdio over Docker on Apple Silicon. The two Know Things builds ran sequentially with the same CPU affinity and evidence budgets. The previous build reproduced its original results on all 104 questions. Each Know Things timing sample contains 208 warm calls. CodeGraph figures come from the prior corpus-matched run.

| Metric | Previous Know Things | With evidence check | CodeGraph |
| --- | ---: | ---: | ---: |
| Sufficient evidence in first hit | 55 of 86 | 53 of 86 | 21 of 86 |
| Sufficient evidence in first five hits | 78 of 86 | 74 of 86 | 36 of 86 |
| Sufficient evidence after one context call | 81 of 86 | 77 of 86 | 64 of 86 |
| Outside-KB questions that returned hits | 18 of 18 | 0 of 18 | 18 of 18 |
| Warm round-trip p50 | 1.606 ms | 1.772 ms | 350.818 ms |
| Warm round-trip p95 | 1.978 ms | 2.759 ms | 696.280 ms |
| Warm round-trip p99 | 2.195 ms | 3.887 ms | 865.816 ms |
| Container CPU time per warm call | 1.17 ms | 1.37 ms | 356.65 ms |

The evidence check abstained on four answerable questions the previous build covered within five hits. The index occupied 1,050,152 bytes in both Know Things runs. The abstention result covers 18 unsupported holdout questions and four false hits remain in the separate development set. The labels still need developer review, and answer quality in real clients remains unmeasured. The native suite passed six tests, including MCP no-match behavior and telemetry. Both temporary benchmark containers and volumes were removed.

## Blinded answer comparison

The follow-up uses 28 fresh questions at the pinned KB revision. Each of 14 topics has one question with reference lines and one question that asks for an undocumented detail about the same topic. The original 104 questions remain the retrieval regression suite. Both systems used five search hits, up to 12 KiB of search text, and up to 4 KiB of context from one top-hit expansion. The same Claude Code model, `claude-sonnet-5-5`, received the same instructions and only the excerpts for its assigned questions. Excerpts used neutral citation IDs. Each of 14 retained responses covered four unrelated topics with randomized system order. One response required a punctuation-only JSON repair. All 56 answer records passed ID and shape validation.

| Preliminary check | Know Things | CodeGraph |
| --- | ---: | ---: |
| Answerable questions with all reference lines after context | 13 of 14 | 9 of 14 |
| Model declared answerable questions answered | 14 of 14 | 9 of 14 |
| Model declared answered and cited all reference lines | 13 of 14 | 9 of 14 |
| Unsupported questions that returned search hits | 9 of 14 | 14 of 14 |
| Model abstained on unsupported questions | 14 of 14 | 14 of 14 |
| Answers with missing required citation IDs or invalid IDs | 0 of 28 | 0 of 28 |

The cited-line check measures exact reference coverage. A blinded reviewer still needs to check whether alternate cited passages support an answer, whether hedged answers address the full question, and whether additional claims are grounded. The private review packet and blank score sheet are under `.local/answer-quality-2026-11-23/review/`. The system key and automated scores are outside that packet. The evidence check remains in place. These results neither establish improved answer-level abstention over CodeGraph nor justify a threshold change.

## 330-document operational comparison

The fixture contains the same 33 KB documents at ten distinct paths per document. A real local Git revision pins each fixture version. CodeGraph's expanded index copies vectors from the stored 33-document baseline. This measures search behavior at the larger index size. Relevance on a genuinely larger corpus and full embedding rebuild costs remain open. Both stacks ran sequentially with the same single-vCPU affinity on Apple Silicon. Each served 48 serial requests and 48 requests in groups of four. Know Things used four MCP stdio sessions. The CodeGraph runner dispatched four asynchronous calls in one search process, using the production search, database, and embedding service.

| Metric | Know Things | CodeGraph |
| --- | ---: | ---: |
| Serial warm round-trip p95 | 2.91 ms | 191.00 ms |
| Four-request batch round-trip p95 | 8.73 ms | 620.12 ms |
| Four-client warm throughput | 673.67 calls/s | 10.71 calls/s |
| Warm container memory | 33.71 MiB | 2,048.13 MiB |
| Database directory | 15.37 MiB | 230.96 MiB |

Both served searches during a malformed update and recovered a new visibility marker. Know Things detected the invalid snapshot, preserved the previous revision, and made the corrected snapshot searchable in 1.01 s with a one-second poll. CodeGraph's isolated ingestion wrapper rejected the malformed input before production ingestion. The follow-up used production ingestion for the changed document. The command completed in 3.76 s with search traffic, and a probe confirmed visibility by 4.38 s. This compares the exercised fixture paths. An operational check of CodeGraph's scheduled source sync remains open. The measurements and repeat are under `.local/answer-quality-2026-11-23/scale/report/`.
