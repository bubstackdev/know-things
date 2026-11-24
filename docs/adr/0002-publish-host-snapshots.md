# ADR 0002. Publish validated source snapshots from the host

**Status** Accepted

## Context

The daemon needs a corpus it can index offline with identifiable source revisions. Source fetches can fail independently, and readers need to keep querying a consistent index during a refresh.

## Decision

Export tracked Markdown from resolved Git commits. Managed sync fetches the KB into a host-side bare mirror and includes explicitly selected GitLab wiki paths. Publish the combined snapshot atomically after all configured sources fetch and validate. A manual Shortcut import builds a separate snapshot from approved Doc bodies and retains each source update time and content digest.

The daemon polls the mounted directory, validates the snapshot again, and commits document changes, deletions, postings, and the indexed revision in one SQLite transaction. It keeps the last good revision available on a failed refresh. [Snapshot code](../../src/source.rs) · [Index code](../../src/index.rs)

## Consequences

Deployments can rebuild from an export with the network disabled. GitLab access and SSH credentials belong to the host sync process. A failed fetch in a multi-source sync delays publication for the whole set and surfaces as stale data. Shortcut Docs need a fresh export and import on each update.

Fetching from inside the daemon would require network access and credentials there. Indexing a working tree directly would include edits outside the resolved Git commit and make the corpus harder to reproduce. [Sources and refresh](../sources.md)
