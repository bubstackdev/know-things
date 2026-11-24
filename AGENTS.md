# Repository Guidelines

## Project Structure

Know Things is a Rust CLI and daemon for local knowledge-base retrieval. The CLI starts in `src/main.rs`. Snapshot export and import live in `src/source.rs` and `src/shortcut.rs`. SQLite indexing and ranking live in `src/index.rs` and `src/relevance.rs`. The daemon and MCP adapter live in `src/daemon.rs` and `src/mcp.rs`. Integration tests live in `tests/cli.rs`. See `config/wiki-sources.json` for wiki configuration, `scripts/` for launch agent templates, `docs/adr/` for current design decisions, and `docs/zak/` for design and benchmark history.

## Build, Test, and Run Locally

Use Rust 1.96 or newer and Git. The documented container workflow uses Docker Desktop on Apple Silicon.

- `cargo build --locked --release` builds `target/release/know-things`.
- `cargo run --locked -- --help` lists the CLI commands.
- `cargo test --locked --all-targets` runs unit and integration tests.
- `docker --context desktop-linux compose up --build -d --wait` builds and starts the daemon after you export a committed KB snapshot and set `KB_SOURCE_DIR`. Follow the setup in `README.md`.

## Style and Testing

Use four-space indentation and run `cargo fmt --all -- --check` to check formatting. Follow the surrounding Rust naming conventions with `snake_case` for functions and modules and `UpperCamelCase` for types.

Use Rust's built-in test harness and `#[tokio::test]` for async integration tests. Name tests for the behavior they verify, for example `managed_sync_keeps_the_export_after_a_failed_fetch`. Use temporary Git fixtures for CLI tests. Run `cargo test --locked --test cli` for the integration suite. The MCP stdio test needs permission to open a local Unix socket.

## Commits and Reviews

Use short, imperative commit subjects, following Git history examples such as `Simplify console response validation`. In merge requests, summarize the behavior change, list verification commands and results, and link a related issue when available.

## Local Data and Sessions

Keep private snapshots, credentials, and evaluation data out of Git. Use `.local/` for private working data. Control only jobs started by your session. Never suspend, terminate, or change the terminal state of another session or its processes.
