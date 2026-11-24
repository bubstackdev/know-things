# Know Things 0.1.0

Know Things runs a shared, network-disabled retrieval daemon on Apple Silicon. This release searches a Markdown knowledge base, selected GitLab wikis, and optionally imported Shortcut Docs. It returns revision-pinned citations through MCP to Codex and Claude Code. Scheduled source refresh and the Know Ops relay run on the Mac.

## Install from the release bundle

You need Docker Desktop, Git, and a local checkout of the KB you can access. The bundle includes the macOS ARM64 binary, Linux ARM64 image, Compose file, and source. Run the following from the directory containing the archive and its checksum.

```sh
archive=know-things-0.1.0-apple-silicon.tar.gz
shasum -a 256 -c "$archive.sha256" &&
  tar -xzf "$archive"
```

After the checksum passes and the archive extracts, load the image and export your KB checkout. Set `kb_repo` and `kb_url` to your checkout and its HTTPS GitLab project URL.

```sh
cd know-things-0.1.0-apple-silicon
docker --context desktop-linux image load --input image.tar
export KB_IMAGE="$(cat IMAGE)"
install -d -m 700 "$PWD/.local"
export KB_SOURCE_DIR="$PWD/.local/source"
kb_repo="$HOME/Code/knowledge-base"
kb_url="https://gitlab.example.org/docs/knowledge-base"
kb_bin="$PWD/bin/know-things"
"$kb_bin" export --repo "$kb_repo" --repo-url "$kb_url" --source-dir "$KB_SOURCE_DIR"
docker --context desktop-linux compose up --no-build -d --wait
docker --context desktop-linux exec know-things /usr/local/bin/know-things status --ready
```

[Connect an MCP client](../README.md#connect-a-client). To schedule refresh or relay health to Know Ops, first [install the host binary at a stable path](operations.md#install-the-host-binary). Follow the [rollback procedure](operations.md#rebuild-upgrade-and-roll-back) if replacing an existing installation. Use a fresh database filename during the upgrade so the older image can still use its original index.

## Build the release bundle

On an Apple Silicon Mac with Rust, Docker Desktop, and `jq`, run the packager from a checkout of the intended source. It uses the version from Cargo and puts the bundle and checksum under `.local/releases/`.

```sh
bash scripts/package-release.sh
```

Check `SOURCE-SHA256SUMS` inside the archive against the release commit before uploading. Verify the bundle by loading its image into Docker, exporting a fixture with its host binary, and checking a revision-pinned citation through MCP in a separate Compose project.
