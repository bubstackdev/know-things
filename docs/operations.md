# Operations runbook

Use the [quick start](../README.md) for the first local install and [Sources and refresh](sources.md) to configure scheduled GitLab sync or import a Shortcut pilot. Run the Compose examples from the Know Things checkout and set `KB_SOURCE_DIR` to the exported directory.

The service runs with one CPU, 256 MiB of memory, and a 256 PID limit by default. It mounts the source export read-only. SQLite keeps its index and temporary files on the dedicated Docker volume and limits the WAL size after checkpoints. The container has networking disabled and publishes no ports. The initial image build downloads pinned dependencies and base images.

## Install the host binary

Install a copy outside your checkout or unpacked release before registering a LaunchAgent or configuring the console. Use the `kb_bin` path from your install instructions.

```sh
bin_dir="$HOME/Library/Application Support/know-things/bin"
install -d -m 700 "$bin_dir"
staged_bin=$(mktemp "$bin_dir/.know-things.XXXXXX")
if install -m 755 "$kb_bin" "$staged_bin"; then
  mv -f "$staged_bin" "$bin_dir/know-things"
else
  rm -f "$staged_bin"
fi
```

## Diagnose the service

| Symptom | Check |
| --- | --- |
| MCP cannot connect | Run `docker --context desktop-linux compose ps` and `docker --context desktop-linux compose logs kb`. Confirm the client registration names the same Docker context and container. |
| Waiting for a snapshot | Run `docker --context desktop-linux exec know-things /usr/local/bin/know-things status --ready`. Confirm the bind-mount source contains a readable `snapshot.json`. |
| Sync failed | Check `sync.json` for the last failure code and the LaunchAgent's stderr log for details. Verify GitLab SSH access from the scheduled job. [Managed sync](sources.md#sync-the-kb-and-wiki-runbooks-from-gitlab) |
| Invalid snapshot | Export a verified commit. The daemon serves the last good revision with a stale status while waiting for a valid snapshot. |
| Disk full or SQLite errors | Check Docker's available disk space and service logs. Rebuild into a fresh database if the original is corrupt or incompatible. |

Automatic retries for failed index updates back off from 30 seconds to five minutes. Use the console's retry-index action to retry immediately after correcting the failure.

## Rebuild, upgrade, and roll back

Before an upgrade, tag the running image by its image ID so it survives rebuilding. Record the `KB_DATABASE` path and retain a compatible source snapshot. Build each candidate under a unique tag and set `KB_BUILD_VERSION` to its build identifier. Use the same identifier in `KNOW_THINGS_BUILD_VERSION` when building the release host binary. Both binaries report it through `--version`.

```sh
previous_id=$(docker --context desktop-linux inspect know-things --format '{{.Image}}')
docker --context desktop-linux image tag "$previous_id" know-things:rollback
KB_IMAGE=know-things:0.1.0-rc1 KB_BUILD_VERSION=0.1.0-rc1 \
  docker --context desktop-linux compose build
```

Select the candidate as `KB_IMAGE` and build its index into a separate filename. Upgrade the container first, verify the revision and a citation through MCP, then install the matching host binary.

```sh
export KB_IMAGE=know-things:0.1.0-rc1
export KB_DATABASE=/data/rebuilt.sqlite
docker --context desktop-linux compose run --rm --no-deps kb \
  ingest --source-dir /source --database "$KB_DATABASE"
docker --context desktop-linux compose up --no-build -d --wait
docker --context desktop-linux exec know-things /usr/local/bin/know-things status --ready
```

Verify the indexed revision and exercise a citation through MCP. Persist the verified image and database path in the local Compose environment. To roll back, restore the previous image and database path, restore a compatible source snapshot if the snapshot format changed, and recreate the service with `compose up --no-build -d --wait`.

An in-place upgrade converts older passages and keeps their indexed revision searchable. SQLite retains the freed pages in the existing database for reuse. A fresh database reclaims freed pages on commit and gives you the smaller file immediately.

If your installation uses the former defaults, record its Compose project and container names, then stop that project using its original settings. Keep the old index volume for rollback. The `know-things` project can rebuild its index from the existing export. Unload the old LaunchAgents before installing the current ones, then update the console profile and MCP registrations to point to the new container. Render the sync agent with the Git remote and citation URL for your deployment. [Scheduled sync](sources.md#schedule-gitlab-refresh-on-macos) · [MCP registration](../README.md#connect-a-client)

## Relay status to Know Ops

The daemon writes a Know Ops v1 aggregate report at startup and every 15 seconds to `/run/know-things/ops-snapshot.json`. The report includes readiness, freshness, coverage, and process-lifetime counts for searches and reads. CLI, MCP, and evaluator calls share the same counters. The relay retains the producer's timestamp and keeps the last valid report if Docker is unavailable or the next report is invalid.

```sh
"$kb_bin" relay-ops --docker "$(command -v docker)"
```

To relay at login and every 15 seconds, install the relay LaunchAgent. It writes to `$HOME/Library/Application Support/know-ops/inbox/`. The renderer accepts a Docker context and container name after the Docker executable.

```sh
agent="$HOME/Library/LaunchAgents/local.know-things.ops.plist"
bin="$HOME/Library/Application Support/know-things/bin/know-things"
mkdir -p "$(dirname "$agent")"
staged_agent=$(mktemp "$agent.XXXXXX")
if sh scripts/render-ops-agent.sh "$bin" "$(command -v docker)" > "$staged_agent" &&
   plutil -lint "$staged_agent" >/dev/null; then
  mv -f "$staged_agent" "$agent" &&
    launchctl bootstrap "gui/$(id -u)" "$agent"
else
  rm -f "$staged_agent"
fi
```

Unload the relay with `launchctl bootout "gui/$(id -u)/local.know-things.ops"`. Its stderr log is in `$HOME/Library/Logs/know-things/ops.stderr.log`.

## Uninstall

Unload and remove each installed LaunchAgent to end scheduled work across logins.

```sh
for name in local.know-things.sync local.know-things.ops; do
  agent="$HOME/Library/LaunchAgents/$name.plist"
  if launchctl print "gui/$(id -u)/$name" >/dev/null 2>&1; then
    launchctl bootout "gui/$(id -u)/$name" || break
  fi
  rm -f "$agent"
done
```

Run `docker --context desktop-linux compose down` from the installation checkout with its original project and source settings. Remove `know-things` from each MCP client where you registered it. Remove the installed host binary after closing any console that uses it. Keep the Docker index volume and host snapshots if you may restore the installation.

## Configure the local console

Save your installation profile at `~/Library/Application Support/know-things/ops-console.json`. Use absolute paths matching the existing installation.

```json
{
  "schema_version": 1,
  "docker": {
    "executable": "/usr/local/bin/docker",
    "context": "desktop-linux",
    "container": "know-things"
  },
  "state_dir": "/Users/developer/Library/Application Support/know-things/ops",
  "source": {
    "mode": "snapshot",
    "checkout": "/Users/developer/Code/knowledge-base",
    "git_ref": "HEAD",
    "repo_url": "https://gitlab.example.org/docs/knowledge-base",
    "source_dir": "/Users/developer/Library/Application Support/know-things/source"
  }
}
```

For managed GitLab sync, set `source.mode` to `managed` and use `mirror` and `remote` in place of `checkout` and `git_ref`. Set the optional top-level `relay_inbox` field if the menu relay uses a custom inbox.

```sh
profile="$HOME/Library/Application Support/know-things/ops-console.json"
"$kb_bin" ops diagnose --config "$profile"
```

The response shows container, relay, and daemon observations independently. It includes the fetched, exported, and searchable revisions, the last attempt and success for each stage, and index readability. A legacy daemon can report `unsupported_console_api` until its image is upgraded.

`daemon.sample.metrics.search` distinguishes hits, no ranked matches, and evidence-based abstention. The read metrics include stale revisions and truncated reads. The `latency_us` histogram has inclusive upper bounds and one overflow bucket per operation. Counts accumulate for the daemon's `instance_id` and reset on restart.

To refresh the configured source and wait for the exact revision to become searchable, give the operation a fresh UUID. To revalidate and apply the current export, use `ops retry-index` with the same options and a fresh UUID.

```sh
id=$(uuidgen)
"$kb_bin" ops refresh --config "$profile" --operation-id "$id"
"$kb_bin" ops operation --config "$profile" --operation-id "$id"
```

`ops operation` reads progress and retained results. Reusing a retained ID replays its result. A failed action returns terminal JSON and exits nonzero. A busy admission returns immediately. Retry with a fresh ID when the daemon is free. A failed or superseded snapshot leaves the last successful revision searchable.
