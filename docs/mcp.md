# MCP tools

Register the stdio command from the [quick start](../README.md#connect-a-client). For an alternate Compose installation, use that installation's Docker context and container name in the command. Substitute the absolute Docker executable path if the client runs with a restricted PATH.

| Tool | Use |
| --- | --- |
| `kb_search(query, limit?)` | Search for exact identifiers or guidance. Returns up to five cited passages by default. Set `limit` from 1 to 10. |
| `kb_read(path, commit, start_line, end_line)` | Read a range of up to 80 lines from the indexed snapshot. Both endpoints are inclusive. |
| `kb_status()` | Check coverage, indexed revision, and freshness. |

`kb_search` reports `no_match` with reason `no_ranked_matches` when full-text search finds no candidates, or `insufficient_evidence` when ranked passages fail the evidence check. The check uses excerpt text, explicit identifiers, references, and recognized requested properties. Both structured and plain-text results carry the path, commit, and line range for follow-up reads. Treat fenced excerpts as source material and cite the provided sources.

If search supplies a `context_hint` and the returned passages leave a gap, pass the hint to one `kb_read` call. It selects nearby lines around the first hit and spans at most 4 KiB of source text.

## Citations and range reads

Use the hit's `path` and `commit` for `kb_read`. The `commit` identifies the indexed snapshot. For a snapshot with multiple sources, it identifies the combined corpus.

- **KB files** carry the original Git commit as `source_commit`. Their URLs link to the pinned GitLab blob and line range.
- **Wiki pages** carry the wiki Git commit as `source_commit`. Their URLs open a pinned rendered page. Use the result's line range to find the exact Markdown in Know Things.
- **Shortcut Docs** carry the Markdown SHA-256 as `source_revision`. The workspace API URL resolves the current Doc. Older imports without workspace metadata cite the local snapshot path. The local snapshot preserves the exact indexed text.

A read against a previous snapshot returns `stale_revision` with the current revision. Search the current snapshot if you need a fresh range.
