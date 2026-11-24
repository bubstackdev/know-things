# ADR 0004. Cite and read evidence at a known revision

**Status** Accepted

## Context

Clients may fetch more context after a search. A background refresh can replace the index between those calls. Git files, wiki pages, and Shortcut Docs use different source identifiers.

## Decision

Each hit includes exact Markdown, an inclusive line range, a document path, and the indexed snapshot revision. Use the hit's `path` and `commit` to request a range. If the index advanced, return `stale_revision` with the current snapshot revision.

GitLab hits carry the Git commit as `source_commit` and link to the pinned blob or rendered wiki page. Shortcut hits carry a Markdown SHA-256 as `source_revision` and link to the Doc's API URL. The local snapshot preserves the exact Shortcut text associated with the digest. [MCP tool contract](../mcp.md)

## Consequences

An excerpt and its follow-up read refer to the same indexed source text. Clients repeat the search after a stale revision response. GitLab links can be opened at the source revision. Shortcut's API link opens the current Doc, so auditing the precise historical wording depends on retaining the export.

A path-only read could return newer lines under an older citation. A current-page URL is useful for navigation, while the source revision and line numbers identify the evidence the client actually received.
