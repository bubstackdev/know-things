# Know Things

Know Things gives Codex and Claude Code cited excerpts from a Markdown knowledge base, selected GitLab wikis, and optional Shortcut Docs. Search runs in a network-disabled Docker daemon on Apple Silicon. Hosted clients may forward excerpts under company policy.

[Install from the 0.1.0 bundle](docs/release.md#install-from-the-release-bundle) if you have the release archive.

## Quick start

You need Docker Desktop, Git, Rust 1.96 or newer, and a local checkout of your knowledge base. Set `kb_repo` to the checkout path and `kb_url` to its HTTPS GitLab project URL. Run the following from the Know Things repository.

```sh
kb_repo="$HOME/Code/knowledge-base"
kb_url="https://gitlab.example.org/docs/knowledge-base"
cargo build --locked --release
kb_bin="$PWD/target/release/know-things"
install -d -m 700 "$PWD/.local"
export KB_SOURCE_DIR="$PWD/.local/source"

"$kb_bin" export --repo "$kb_repo" --repo-url "$kb_url" --source-dir "$KB_SOURCE_DIR"
docker --context desktop-linux compose up --build -d --wait
docker --context desktop-linux exec know-things /usr/local/bin/know-things status --ready
```

### Connect a client

Use the command for your client.

```sh
codex mcp add know-things -- docker --context desktop-linux exec -i know-things /usr/local/bin/know-things mcp
claude mcp add --transport stdio --scope user know-things -- docker --context desktop-linux exec -i know-things /usr/local/bin/know-things mcp
```

Try asking about a setting documented in your knowledge base. Use [the MCP tools](docs/mcp.md) to search, read more lines, or check freshness.

## Guides

- [Sources and refresh](docs/sources.md) covers GitLab sync, wiki selection, refresh intervals, and Shortcut imports.
- [Operations runbook](docs/operations.md) covers diagnostics, upgrades, rollback, and Know Ops.
- [Retrieval evaluation](docs/evaluation.md) covers the evaluator, test suite, and CodeGraph comparison.
- [Architecture decision records](docs/adr/README.md) explain the design and where its tradeoffs matter.
