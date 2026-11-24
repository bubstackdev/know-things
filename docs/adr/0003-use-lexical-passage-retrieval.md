# ADR 0003. Use lexical passage retrieval

**Status** Accepted

## Context

The corpus contains long Markdown files with API names, tickets, configuration keys, and procedures. Developers need a short excerpt that answers the question. The release comparison includes first-response evidence, evidence after one follow-up, and the resource cost of the complete retrieval stack.

## Decision

Use SQLite FTS5 with BM25 over document titles, heading ancestry, passage text, and split identifier terms. Preserve exact Markdown line ranges during parsing. Prefer exact identifiers and sections that describe the requested procedure when the query makes the action clear. Check evidence before returning hits and suggest one bounded follow-up range when nearby lines can help. [Index and ranking](../../src/index.rs) · [Evidence checks](../../src/relevance.rs)

Store Markdown once per document. Passages record line numbers and UTF-8 byte ranges, and a contentless FTS5 table keeps the search index and BM25 statistics. Retrieve exact excerpts from the document after selecting the ranked candidates. Enable automatic page reclamation on fresh databases so deletions and edits can release unused disk space. [SQLite FTS5 storage options](https://www.sqlite.org/fts5.html#contentless_delete_tables)

## Consequences

The current runtime fits in one Rust process and one SQLite database. The original matched 33-document benchmark found sufficient evidence within five hits for 78 of 86 answerable questions with Know Things and 36 with CodeGraph. Those figures describe the original benchmark build and corpus. Paraphrases with little lexical overlap and questions that require reasoning across documents remain important test cases. [Matched benchmark history](../zak/plans/2026-11-23-local-kb-rag.md#matched-run-results)

Dense or hybrid retrieval could help with paraphrases and would add a model, embedding work, and another runtime cost to measure. Revisit that decision if a frozen, representative evaluation shows a quality gain that justifies those costs. [Evaluation guide](../evaluation.md)
