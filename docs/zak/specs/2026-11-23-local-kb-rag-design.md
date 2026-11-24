# Local RAG design for a Markdown knowledge base

This is the original design and release-gate proposal. See the [accepted ADRs](../../adr/README.md) and [operations runbook](../../operations.md) for current behavior.

Run one Rust daemon in a Linux arm64 Docker container per developer Mac. Keep Markdown retrieval and any embedding inference inside that container. Use SQLite FTS5 and section-aware retrieval as the first candidate. Ship whichever candidate demonstrates better retrieval quality, latency, and resource use than CodeGraph RAG on the KB workload. Codex and Claude Code receive cited excerpts through MCP stdio and may pass them to hosted models under company policy.

## Corpus and CodeGraph baseline

The benchmark checkout contains 33 tracked Markdown files, 233,839 bytes, and 3,632 lines. It documents engineering practices and conventions. Long documents mix prose, commands, fenced examples, and tables, so the result needs to identify the relevant passage even when the document is obvious. CodeGraph already provides hybrid search over structured docs and repository READMEs, plus context expansion through `rag_context`.

Three live `rag_search` calls against CodeGraph's structured-doc corpus and a local document-level SQLite probe exposed where to focus the comparison. Both found the expected files for an analyzer rule ID and `AddMassTransit`. For “How do I turn off one analyzer rule for an entire job host?”, CodeGraph ranked the relevant Job Host section third. Its five search excerpts omitted the setting. A separate context call on the third hit returned it. The local SQLite probe found the right file first and established document-level recall only. CodeGraph reported search execution times of 58–238 ms across the three calls. The live results used different source commits, so these checks establish neither performance percentiles nor comparative accuracy.

## Daemon and MCP transport

```text
GitLab main → macOS Git sync → read-only source snapshot
                                      ↓
                            Docker arm64 container
                        daemon → SQLite named volume
                           ↑
                      Unix socket
                           ↑
Codex or Claude Code ← MCP stdio → docker exec -i → MCP adapter
```

The daemon owns ingestion, the index, and query execution. Each client launches a lightweight MCP adapter in the running container using `docker exec -i`. The adapter uses the official Rust MCP SDK and forwards typed requests over a Unix socket with restricted filesystem permissions. Each client gets its own stdio session, and one shared index serves all sessions. Standard output carries only MCP messages. A client disconnect cancels its work and leaves the daemon running. Propagate deadlines end to end and bound concurrent searches, response sizes, and the request queue. [Docker exec](https://docs.docker.com/reference/cli/docker/container/exec/), [MCP stdio transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports), [Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk).

Example registration commands for the Docker Desktop context. Use the absolute Docker executable path where the client has a restricted PATH.

```sh
codex mcp add know-things -- docker --context desktop-linux exec -i know-things /usr/local/bin/know-things mcp
claude mcp add --transport stdio --scope user know-things -- docker --context desktop-linux exec -i know-things /usr/local/bin/know-things mcp
```

[Codex MCP configuration](https://learn.chatgpt.com/docs/extend/mcp?surface=cli), [Claude Code MCP configuration](https://code.claude.com/docs/en/mcp).

The container runs as a non-root user with `network_mode: none`, a read-only root filesystem, and no published ports. Mount the export directory read-only. Put the Unix socket on a private tmpfs and the SQLite database on a Docker-managed volume. The service receives neither the Docker socket nor Git credentials. Install from an approved, pinned arm64 image. Keep raw queries, excerpts, and credentials out of service logs. Treat Markdown as untrusted evidence. Index only approved repository paths and serve only indexed content. [Docker none network driver](https://docs.docker.com/engine/network/drivers/none/).

Publish Know Ops v1 aggregate health and usage in a private tmpfs file at startup and every 15 seconds. A host-side relay optionally copies the latest report to the per-user inbox through Docker exec. The relay validates the payload and preserves the producer's timestamp. Usage is counted once per terminal search or read outcome. Report failures leave retrieval available. Include the publisher and relay in resource measurements for the complete installed service. [Know Ops telemetry contract](../../../../know-ops/docs/telemetry-v1.md).

## Git sync and index updates

Use a documented local snapshot mode and a managed sync mode. A host command can seed the index from a commit in the existing checkout and refresh it offline. In managed mode, a macOS user LaunchAgent fetches `main` from the configured GitLab repository every five minutes into a dedicated Git repository using approved host credentials. It exports regular tracked Markdown blobs directly from one resolved commit, with a manifest of paths and blob IDs. Write a bounded JSON snapshot to a temporary file and rename it into place on the same filesystem. The service polls the mounted directory. Compare the indexed commit, latest export, and last successful upstream check to distinguish stale content from a quiet upstream. Local snapshot mode reports upstream freshness as unknown. [Git tree enumeration](https://git-scm.com/docs/git-ls-tree).

Validate the export before indexing. Reject unsafe or duplicate paths, unsupported file modes, malformed files, and bundles beyond configured limits with an actionable error. Compare the new manifest to the last indexed one. Parse changed files, reuse compatible data for unchanged files, and remove deleted or renamed entries. Prepare the update off the query path. A single SQLite transaction commits the source revision, postings, and any vectors together. Each query reads one committed snapshot. Use a patched, pinned SQLite release with FTS5 and WAL on the Docker volume. On a failed fetch or index build, keep serving the last successful revision with freshness and error status. Bootstrap with no successful snapshot returns an explicit unavailable status. [SQLite WAL](https://www.sqlite.org/wal.html).

Make the index disposable. Deliver Compose configuration and local commands for installation, status, sync, rebuild, upgrade, and rollback. Pin a compatible image and index schema. Stage any incompatible rebuild in a separate database and retain the prior usable database through first health verification. The runbook covers Docker unavailable, Git auth failure, stale export, disk exhaustion, and corrupt index. A rebuild starts from the last verified export. Docker restart policy starts the daemon when the local Docker engine is available.

## Retrieval and evidence

Use a Markdown parser with source spans and keep the original text. Index canonical sections and smaller passages with document title, heading ancestry, and typed identifiers such as API names, diagnostic IDs, and config keys. Keep list items, tables, and code blocks together when possible. Split oversized blocks on source lines and return the closest relevant span with its supporting context. Every excerpt carries an inclusive line range in the original blob. [pulldown-cmark](https://docs.rs/pulldown-cmark/latest/pulldown_cmark/struct.Parser.html).

| Candidate | Design | Decision criterion |
| --- | --- | --- |
| Lexical | FTS5 BM25 over titles, headings, and body. Boost exact identifier matches and tokenize case changes, punctuation, and dotted names for general matching. Test limited query expansion for recurring terminology in the corpus. | Use if it passes the comparison. It requires one Rust service and SQLite. |
| Hybrid | Add exact cosine search over locally generated embeddings, then fuse ranked lists. Test an identifier fast path and use semantic retrieval for queries where the lexical path performs poorly. | Adopt only if the quality gain generalizes to the holdout and the combined system still clears the latency and memory gates. |
| Reranking | Score only the top few passages with a local reranker after retrieval. | Test only if judged evidence is already in the candidate pool and ranking is the remaining source of errors. Count its runtime and model memory in the comparison. |

Start any dense experiment with a pinned BGE-small-en-v1.5 GGUF artifact and a CPU-only `llama.cpp` worker bound to loopback inside the same network-disabled container. Package the verified weights with the release and use the model tokenizer, query preprocessing, and pooling configuration. Run exact search before considering an approximate index. At 384 dimensions, 10,000 float32 vectors take about 15 MiB before storage overhead. Measure the full process tree, including the model and runtime. On model failure, serve lexical results and report the degraded mode. The build and runtime require no Python. [SQLite FTS5](https://www.sqlite.org/fts5.html), [BGE model card](https://huggingface.co/BAAI/bge-small-en-v1.5), [llama.cpp embeddings](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md#post-v1embeddings-openai-compatible-embeddings-api).

| MCP tool | Contract |
| --- | --- |
| `kb_search(query, limit?)` | Return up to five ranked, self-contained evidence passages by default with title, section, exact Markdown, path, line range, commit, immutable GitLab URL, and freshness and retrieval status. Cap the total evidence budget. Return a distinct no-match status when nothing clears the evidence threshold calibrated on the tuning set. |
| `kb_read(path, commit, start_line, end_line)` | Read a bounded range from the indexed revision. Return an explicit stale-revision error when the index has moved and include the current revision. The original citation remains pinned to the original commit. |
| `kb_status()` | Report indexed commit, latest exported commit, last successful upstream check, index mode, coverage, and last failure. Keep refresh and rebuild operations in the local admin CLI. |

Return structured tool results and a compact text rendering for client compatibility. Give the clients instructions to cite the supplied URLs, report gaps in evidence, and treat quoted commands and instructions as source material. Use commit-pinned links of the form `/-/blob/<commit>/<path>#L<start>-<end>`. [MCP tool results](https://modelcontextprotocol.io/specification/2025-11-25/server/tools).

## Release gates against CodeGraph

Acquire two baselines. Use the live CodeGraph MCP tool for the latency and evidence developers experience today. Use a corpus-matched CodeGraph instance for causal comparison, with both systems built from the same immutable KB commit and the same retrieval scope. The live tool exposes a source-kind filter. A result filter applied after its ranking would change the experiment by dropping candidates after retrieval. Record source revisions, parameters, evidence budgets, and tool versions. For answers, use the same model and prompt with each system's retrieved evidence.

Build a private set of at least 100 developer questions with exact-identifier lookups, paraphrases, multi-section questions, version-specific guidance, and questions with no KB answer. Have developers label sufficient evidence by source line span and grade answer faithfulness to cited passages without seeing which system retrieved them. Keep a held-out set outside tuning. Report paired results by question type and overall, with uncertainty intervals. For questions that need more than one passage, score whether the combined evidence can answer the question.

| Axis | Release comparison |
| --- | --- |
| Retrieval and answer quality | Improve sufficient evidence in the top result and top five, grounded answer correctness, and unsupported-answer behavior. Score first response separately from search plus context, with the same token and call budgets. Require valid source lines and immutable citations on every result. |
| Latency and throughput | Improve p50, p95, and p99 client round-trip time and sustained throughput at the same concurrency and error budget. Measure cold startup and first query, warm queries, and background refresh separately. Time search and time to sufficient context. Record server-only timing separately from the hosted network path. |
| Memory, CPU, and disk | Reduce steady and peak memory, CPU time per query and rebuild, and installed and indexed bytes. Include the model, adapter processes, SQLite pages, and indexing peaks. Measure container process and cgroup costs plus the host's incremental Docker VM footprint. Compare equivalent per-instance and per-active-user costs separately. |
| Freshness and reliability | Improve update and rebuild time and measure commit-to-search visibility. Test restarts, concurrent clients, offline use, source edits and deletes, failed updates, and corrupt snapshots. Check that old data is labeled and failed refreshes do not publish partial results. |

Measure on a representative Apple Silicon Mac, including the oldest supported tier, with fixed Docker resource settings. Run repeated samples on a dedicated test machine or during a reserved quiet window. Exercise the current corpus and a 10× growth fixture for indexing, disk, and query costs. Include single-client, concurrent-client, and refresh-under-load runs. Shared hosted CodeGraph graphs, caches, and runtime span other workloads. A scoped CodeGraph deployment or maintainer-provided measurements are required to establish comparable memory, CPU, and disk figures. Report live-client observations with their deployment context. Any unmeasured axis, statistical tie, or regression leaves the all-axis claim open.

## Build and rollout order

1. Capture the CodeGraph baseline at a frozen KB revision and build the evidence-scoring harness. Label tuning and held-out questions before tuning.
2. Implement commit export, section-aware FTS5 indexing, local benchmark commands, and citation checks. Evaluate the lexical baseline against CodeGraph. Run the hybrid experiment if lexical retrieval misses material evidence.
3. Implement the daemon, MCP adapter, Compose packaging, and operational commands. Test registration and tool calls in real Codex and Claude Code sessions. Include daemon restart, concurrent clients, read-only mounts, network isolation, and index recovery in the integration checks.
4. Measure the complete installed service and run a developer pilot with the pinned build. Promote it as the default KB tool once the comparison passes and the team has exercised rollback. Keep the benchmark runnable for every retrieval, model, or index change.
