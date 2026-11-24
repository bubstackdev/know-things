# ADR 0001. Run one offline Rust daemon per Mac

**Status** Accepted

## Context

Codex and Claude Code need separate MCP stdio sessions. Index refresh and SQLite state need a lifetime independent of those sessions. Apple Silicon is the deployment target, and retrieval must work offline after a source export.

## Decision

Build one Rust executable for macOS arm64 host commands and Linux arm64 container commands. Run one `serve` daemon per installation. Each MCP client starts `know-things mcp` with `docker exec -i`. The adapter forwards typed requests to a private Unix socket in the container. The daemon owns ingestion, search, and the shared index.

Docker runs the daemon as a non-root user with networking disabled, a read-only root filesystem, a read-only source mount, and a dedicated writable index volume. The socket lives on private tmpfs. Git sync and its credentials stay on the host. [Compose configuration](../../compose.yaml)

## Consequences

Client sessions share the index and a refresh worker. Ending a client session leaves the daemon available to other clients. Installation requires Docker Desktop. Each client needs permission to run the configured Docker command. Hosted clients may forward returned excerpts under company policy.

A daemon per MCP session would couple indexing and readiness to client lifetimes. An HTTP MCP listener would add a listening endpoint and its access-control requirements. The stdio adapter provides the client transport while the Unix socket stays private to the container.
