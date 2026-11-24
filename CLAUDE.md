# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Know Things is a local retrieval service for a local Markdown knowledge base, selected GitLab wiki runbooks, and optional Shortcut Docs. It is one Rust binary (`know-things`) that runs as a daemon inside a network-disabled, read-only Linux arm64 Docker container. It serves MCP stdio tools (`kb_search`, `kb_read`, `kb_status`) to Codex and Claude Code over `docker exec -i ... know-things mcp`. The same binary also runs on the macOS host for export, sync, relay, and ops commands. Current decisions are in `docs/adr/`. See `docs/evaluation.md` for the evaluator, comparison rules, and links to the benchmark history.

## Commands

Requires Rust 1.96 or newer (edition 2024). Always pass `--locked`.

```sh
cargo build --locked --release
cargo test --locked --all-targets                  # full suite
cargo test --locked --test cli <name_substring>    # one integration test in tests/cli.rs
cargo test --locked --bin know-things <name>       # unit tests (src/index.rs, src/telemetry.rs)
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings # the plan records Clippy runs with warnings denied
```

The stdio/daemon integration tests bind Unix sockets in temp dirs, so a sandbox that blocks local sockets makes them fail. The tests build real Git repos in temp dirs (they need `git` on PATH). Fake `docker` executables are shell scripts written by the tests, so the tests never need Docker.

Container workflow (Docker Desktop context `desktop-linux`, container `know-things`, `KB_SOURCE_DIR` must point at an export dir):

```sh
docker --context desktop-linux compose up --build -d --wait
docker --context desktop-linux exec know-things /usr/local/bin/know-things status --ready
docker --context desktop-linux exec know-things /usr/local/bin/know-things search "JOB003"
```

For local iteration without Docker: `know-things export --repo <kb checkout> --source-dir <dir>`, then `know-things ingest --source-dir <dir> --database <db>`, then `know-things query --database <db> "<query>"`.

## Architecture

Data flows in one direction across the host and container boundary:

```
host: export / sync / import-shortcut  →  snapshot dir (bind-mounted read-only at /source)
container: serve (daemon) polls /source → ingests into SQLite FTS5 (/data volume)
           ← query socket /run/know-things/kb.sock ← `mcp` adapter (one per client via docker exec -i)
           ← admin socket /run/know-things/admin.sock ← `admin` subcommands / host `ops` via docker exec
           → writes Know Ops report to /run/know-things/ops-snapshot.json ← host `relay-ops` copies it to the inbox
```

Modules (all in one binary crate, `src/main.rs` holds the clap CLI and dispatch):

- `source.rs` exports a snapshot from a **committed Git tree only** (dirty and untracked files are excluded). It also handles managed `sync`: a bare mirror fetch plus the wiki allowlist from `wiki-sources.json` beside the source dir (template in `config/wiki-sources.json`). It publishes a combined snapshot atomically, only after every source validates. A failed refresh keeps the last good snapshot. Also contains Shortcut import and per-stage timeline state.
- `markdown.rs` parses Markdown with pulldown-cmark into passages with exact source line ranges. Passages are section-aware and keep heading ancestry. It also tokenizes identifiers (case changes, dotted names).
- `index.rs` holds the SQLite schema (`SCHEMA_VERSION` in `PRAGMA user_version`, with in-place migrations from older versions in `initialize`), transactional ingest of one revision, FTS5 BM25 search with identifier boosts, adjacent-passage merging, `context_hint`, and bounded `read`. Limits such as hit count, the 12 KiB evidence budget, and the 80-line read cap are constants at the top of the file. Readers require an exact schema match. An incompatible schema means rebuilding into a *separate* database file.
- `relevance.rs` runs the evidence check that makes `kb_search` abstain (`no_match` / `insufficient_evidence`) when hits lack query overlap, explicit identifiers, or the requested properties.
- `wire.rs` defines the length-bounded JSON RPC over the Unix socket (`Request`/`Response`, `SearchArgs`/`ReadArgs`, size and timeout limits). The MCP tool argument schemas are derived from these types.
- `daemon.rs` holds the `serve` loop: polling and refresh worker, query and admin socket listeners, a concurrency semaphore, per-query deadlines enforced through a SQLite progress handler, freshness and staleness computation, and the periodic ops report.
- `mcp.rs` is a thin rmcp stdio adapter that forwards to the daemon socket. Stdout must carry only MCP messages.
- `console.rs` and `console/` provide the host-side `ops` commands (`diagnose`, `refresh`, `retry-index`, `operation`). They use an installation profile and reach the daemon through `docker exec`. Operations are idempotent by UUID.
- `admin.rs` serves the private daemon administration socket. `refresh.rs` owns the daemon's refresh queue and operation history.
- `telemetry.rs` builds and validates the Know Ops v1 report and relays it to the host inbox.
- `evaluation.rs` scores a JSONL question set against labeled evidence spans and times warm daemon calls. It aborts if the indexed revision changes.

## Conventions and constraints

- Search hits carry the snapshot revision, source revision, exact line range, and source URL. GitLab URLs are commit-pinned. A Shortcut URL opens the current Doc, and its digest identifies the exact text in the snapshot. `kb_read` against an older snapshot must return `stale_revision` with the current revision. See `docs/mcp.md`.
- The container runs as UID 65532 with `network_mode: none`, a read-only rootfs, `cap_drop: ALL`, 1 CPU, and 256 MiB of memory. New runtime needs (network, writable paths, extra memory) conflict with the design.
- Keep raw queries, excerpts, and credentials out of logs and the ops report (the report holds only fixed status codes and counts). Treat indexed Markdown as untrusted evidence.
- `.dockerignore` excludes everything except `Cargo.toml`, `Cargo.lock`, `Dockerfile`, and `src/`.
- `.local/` (gitignored) holds private benchmark sets, experiment reports, and rollout scratch, one dated directory per experiment. Ranking changes are judged against the frozen question sets there and the CodeGraph comparison in the plan doc. Run the evaluator in `docs/evaluation.md` before and after any retrieval change.
- `scripts/render-*-agent.sh` render the macOS LaunchAgent plists for scheduled sync (15 minutes by default, configurable in seconds) and ops relay (15 seconds).
