# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

**Read [AGENTS.md](AGENTS.md) first.** It holds the binding product direction (documentation-only scope, on-device inference only, read-only sources, read-only `../flash`) and the permanent rule to never suspend or interfere with other terminal sessions' processes, including for benchmark isolation. [README.md](README.md) is the operator guide and documentation map. Update the owning doc (`docs/ARCHITECTURE.md`, `docs/CONTRACTS.md`, …) when a material decision changes.

## Commands

Rust 1.98+, Unix host. SQLite is bundled; all dependencies are exact-pinned (`=x.y.z`) in `Cargo.toml`.

```sh
cargo build --locked                                   # add --offline when deps are cached
cargo test --locked -- --test-threads=1                # timing-sensitive tests; run serialized
cargo test --locked --test lifecycle                   # one integration test file (tests/lifecycle.rs)
cargo test --locked --test lifecycle some_test_name    # one test
cargo clippy --locked --all-targets -- -D warnings
```

Python checks (stdlib `unittest`, no model assets needed unless stated):

```sh
PYTHONDONTWRITEBYTECODE=1 python3 tests/evaluation.py --binary target/debug/know-things
PYTHONDONTWRITEBYTECODE=1 python3 tests/answer_evaluation.py
```

Other `tests/*.py` (packaging, embedding_bundle, runtime_capture, native_diagnostics) and `scripts/check_*.py` exercise packaging and real model assets; run `--help` for arguments. Run a coordinator against a data dir outside any source tree, e.g. `target/debug/know-things local .private/data | cat` (resident stdio modes reject a terminal stdout). Request examples are in README.md.

`.private/` (gitignored) holds stores, captured model/runtime/Flash artifacts and evaluation data. Never commit it; lifecycle tests must use disposable fixtures, never a real documentation source.

## Architecture

One resident Rust coordinator (`src/coordinator.rs`) owns a data directory through a process-lifetime lock and implements every typed operation (register, reconcile, search, context, answer, exclude/include, prune, backup, rebuild_lexical, policies, status). CLI (`src/main.rs`: `serve`, `request`, `local`, `mcp`, `mcp-stdio`, `restore`, `assets`, `flash-probe`) and MCP are thin adapters via `src/transport.rs`; don't add per-interface logic. Wire types and JSON schema live in `src/contracts.rs`.

- **Store (`src/store.rs`)**: SQLite is canonical (WAL, `synchronous=FULL`). Migrations are `migrations/NNN.sql` applied in sequence by `PRAGMA user_version` in `Store::open`; schema 5 is a Rust migration (`vector_store::migrate`). New schemas must be forward-only, transactional, refused by older binaries, accepted by `restore`, and covered by the upgrade tests (`tests/support/mod.rs` builds legacy layouts).
- **Revisions and visibility**: parsing (`parser.rs`) and embedding (`embedding.rs`) happen outside the store lock in `ingestion.rs`; each document then publishes atomically — revision, chunks, FTS5 rows, vector mappings and the corpus generation advance in one transaction. FTS is never repaired by a background task. Citations reference immutable revision byte spans, so they resolve even after source edits.
- **Retrieval**: lexical FTS5 plus exact dense similarity (`vector_store.rs`) over the same read snapshot; vectors are content-addressed by chunk-text hash + full embedding-space identity and shared across collections, with per-source eligibility. Exclusion/policy (`source_policy.rs`) is an eligibility boundary checked at query time. No ANN — exact search is the correctness oracle.
- **Answers**: `evidence_selection.rs` → `evidence_render.rs` → `generation.rs` drives a persistent native Flash worker (`flash.rs`, `process.rs`; worker source `workers/flash.mm`) over framed JSON pipes, one active request, streaming with cancellation and deadlines; `citations.rs` groups/validates cited sources. Pending answers lease their evidence revisions against pruning.
- **Background**: `periodic.rs` (scan interval + retention) and `watch.rs` (macOS native hints) share one maintenance worker and the manual ingestion limit; both are opt-in per collection policy.
- **Assets** (`assets.rs`, `integrity.rs`): models, runtimes, tokenizers and the Flash artifact are content-hashed bundles built by `scripts/bundle_*.py` / `capture_*.py` and verified before load. The embedding worker is `workers/embedding.py`. Never depend on a mutable `../flash` checkout.

Use `crate::hash` / `crate::identity` (length-framed SHA-256 in `src/lib.rs`) for any new identity.
