# ADR 0005. Relay aggregate health and keep administration local

**Status** Accepted

## Context

Developers need to see whether the daemon is ready, whether sources are stale, and which refresh stage failed. Know Ops consumes a local aggregate report. Operators also need to request a refresh and verify that the intended revision became searchable.

## Decision

Publish a Know Ops v1 aggregate report to private tmpfs at daemon startup and every 15 seconds. The optional macOS relay validates the report and copies it to the local inbox while preserving the producer's timestamp. Report health, coverage, status codes, and aggregate usage. Keep raw queries, excerpts, and credentials out of the report.

The local console and `know-things ops` use a separate admin socket for refresh and retry operations. A managed refresh tracks fetch, export, and index stages. A retry applies the current export. A retained operation ID returns the recorded result. MCP clients use the search, read, and status tools. [Operations runbook](../operations.md)

## Consequences

The optional relay can fail while retrieval remains available. The last valid report retains its original timestamp, which lets the consumer detect an old sample. An admin refresh can confirm the intended searchable revision through the same index worker used for scheduled refreshes.

Putting control actions on the MCP surface would make them callable by retrieval clients. Remote telemetry would add a network dependency. The file relay and admin socket keep both workflows on the developer's machine.
