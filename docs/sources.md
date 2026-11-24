# Sources and refresh

Use a source directory beneath a private parent with mode `0700`. The exporter sets permissions on the source directory and snapshot so the container's non-root user can read them. Keep snapshots and credentials in local storage outside Git.

## Export a local Git checkout

Run the [quick start](../README.md) or follow the [bundle install guide](release.md) to export the checkout's committed `HEAD` and set `kb_bin` and `KB_SOURCE_DIR`. To export the GitLab revision already fetched to the checkout, pass `--git-ref refs/remotes/origin/main`. Rerun `export` to update an offline snapshot. The daemon checks for snapshot changes every 15 seconds.

## Sync the KB and wiki runbooks from GitLab

Run managed sync once with the SSH credentials of the account that will run the LaunchAgent. Sync uses a dedicated bare mirror on the Mac. Set `kb_remote` to the Git remote to fetch and `kb_url` to the HTTPS GitLab project URL used for citations.

```sh
kb_remote="git@gitlab.example.org:docs/knowledge-base.git"
kb_url="https://gitlab.example.org/docs/knowledge-base"
"$kb_bin" sync \
  --remote "$kb_remote" --repo-url "$kb_url" \
  --mirror "$PWD/.local/kb.git" --source-dir "$KB_SOURCE_DIR"
```

To include wiki runbooks, copy the example allowlist next to the source directory, edit it to select your wikis, and run sync again. Each entry selects a GitLab group or project. Each `paths` value selects an exact Markdown path or a directory prefix ending in `/`.

```sh
install -m 600 config/wiki-sources.json "$(dirname "$KB_SOURCE_DIR")/wiki-sources.json"
# Edit the installed wiki-sources.json before syncing.
"$kb_bin" sync \
  --remote "$kb_remote" --repo-url "$kb_url" \
  --mirror "$PWD/.local/kb.git" --source-dir "$KB_SOURCE_DIR"
```

Wiki sync follows each repository's default branch and connects over SSH to the host in `--repo-url`. The combined snapshot publishes after every configured source fetches and validates. To remove wiki sources, replace the allowlist with `[]` and run sync.

### Schedule GitLab refresh on macOS

The LaunchAgent starts at login and checks every 15 minutes by default. Pass an optional final argument in seconds to change the interval. For example, use `300` for five minutes. Install the binary to the [stable host path](operations.md#install-the-host-binary) before registering the agent.

```sh
agent="$HOME/Library/LaunchAgents/local.know-things.sync.plist"
bin="$HOME/Library/Application Support/know-things/bin/know-things"
mkdir -p "$(dirname "$agent")"
staged_agent=$(mktemp "$agent.XXXXXX")
if sh scripts/render-sync-agent.sh \
     "$bin" "$PWD/.local/kb.git" "$KB_SOURCE_DIR" "$kb_remote" "$kb_url" > "$staged_agent" &&
   plutil -lint "$staged_agent" >/dev/null; then
  mv -f "$staged_agent" "$agent" &&
    launchctl bootstrap "gui/$(id -u)" "$agent"
else
  rm -f "$staged_agent"
fi
```

To change the interval for an existing agent, render the new plist, then unload and load it with these commands. The first command also stops scheduled refresh when run on its own.

```sh
launchctl bootout "gui/$(id -u)/local.know-things.sync"
launchctl bootstrap "gui/$(id -u)" "$agent"
```

Use `launchctl print "gui/$(id -u)/local.know-things.sync"` to check the agent's last exit code. Find its stderr log at `$HOME/Library/Logs/know-things/sync.stderr.log`. The agent uses the launchd environment, so verify Git and SSH access from a scheduled run. Configure SSH for that account through its SSH config and approved credential store.

Managed refresh reports stale data after a sync failure, when a newer snapshot is waiting to be indexed, or when the last successful upstream check is older than three scheduled intervals. Older sync records without a configured interval use a 15-minute threshold. The daemon serves the last good index during a failed refresh. A manual snapshot reports upstream freshness as unknown.

Export skips individual Markdown entries with unsupported paths, file modes, encodings, or sizes and logs a skip count. Keep the `gitlab-wikis/` and `shortcut-docs/` paths reserved for sources maintained by the importer.

The host keeps the sync status in the original `sync.json` format and records the configured interval in `source-observations.json`. Upgrade the daemon before lengthening the sync schedule beyond the threshold supported by the previous image. If the daemon cannot read sync metadata, check its logs and the `freshness.last_sync_error` field. A valid refresh clears the error.

## Import approved Shortcut Docs

Save a private JSON array of approved Docs from one workspace with the fields `id`, `title`, `updated_at`, and `content_markdown`. Set `--workspace` to that workspace's slug. Import the Docs into a separate snapshot derived from the current KB and wiki export.

```sh
"$kb_bin" import-shortcut \
  --workspace example-team \
  --base-dir "$KB_SOURCE_DIR" \
  --source-dir "$PWD/.local/shortcut-source" \
  --docs "$PWD/.local/approved-shortcut-docs.json"
```

Use the image built in the quick start to try the expanded corpus in a separate Compose project and container. The pilot gets its own index volume.

```sh
KB_SOURCE_DIR="$PWD/.local/shortcut-source" KB_CONTAINER_NAME=know-things-shortcut \
  docker --context desktop-linux compose -p know-things-shortcut up -d --wait
```

Each import replaces the whole selected set and records each Doc's update time and Markdown SHA-256. Repeat the import with a refreshed export to update the pilot. Managed GitLab sync refreshes the base snapshot. See how to [read the exact indexed Shortcut text](mcp.md#citations-and-range-reads).

Older imports without workspace metadata remain searchable. Their citations use the local snapshot path. Reimport with `--workspace` to restore external links.
