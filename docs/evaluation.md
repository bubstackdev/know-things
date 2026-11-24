# Retrieval evaluation

Save question sets under `.local/`. Use one JSON object per line, with an ID, a query, and evidence sets. Each set lists all line spans needed to answer the question. Supply multiple sets for acceptable alternatives. Use an empty array when the pinned corpus lacks the requested answer.

```json
{"id":"example","query":"How are jobs scheduled?","evidence_sets":[[{"path":"guide.md","start_line":12,"end_line":15}]]}
```

Read the active snapshot revision from the daemon and run the evaluator against it.

```sh
revision=$(docker --context desktop-linux exec know-things /usr/local/bin/know-things status --ready | jq -er .indexed_commit)
docker --context desktop-linux exec -i know-things /usr/local/bin/know-things evaluate \
  --questions - --commit "$revision" --repetitions 20 < .local/questions.jsonl
```

The evaluator reports sufficient evidence at ranks one and five, unsupported questions that returned hits, and warm serial daemon latency. It stops if the indexed revision changes. Keep the same pinned question sets and corpus when comparing ranking changes.

For a CodeGraph comparison, give both systems the same source text and evidence budget. Measure first-response evidence separately from evidence after a follow-up read. Record client round-trip time and count the complete retrieval stack when comparing resources. Keep source material, model answers, and blinded review keys private. See the [release gates](zak/specs/2026-11-23-local-kb-rag-design.md#release-gates-against-codegraph), [experiment history](zak/plans/2026-11-23-local-kb-rag.md), and [retrieval ADR](adr/0003-use-lexical-passage-retrieval.md).

Run the native test suite with `cargo test --locked --all-targets`. The stdio integration test needs permission to open a local Unix socket.
