use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::tempdir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command as AsyncCommand},
    time::{sleep, timeout},
};

const REPO_URL: &str = "https://gitlab.example.org/docs/knowledge-base";

fn command_output(command: &mut Command) -> String {
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

fn git_command(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_COUNT")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.org",
        ]);
    command
}

fn git(repo: &Path, args: &[&str]) -> String {
    command_output(git_command(repo).args(args))
}

fn cli_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_know-things"));
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_COUNT");
    command
}

fn cli(args: &[&str]) -> Value {
    let text = command_output(cli_command().args(args));
    serde_json::from_str(&text).unwrap()
}

#[test]
fn index_a_git_commit_retrieve_exact_evidence_and_replace_deleted_content() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    fs::write(repo.join("job guide.md"), "# Jobs\n\n## Host settings\n\nSet `dotnet_diagnostic.JOB003.severity = none` in .editorconfig.\n").unwrap();
    fs::write(
        repo.join("other.md"),
        "# Schedules\n\nA schedule describes when a job runs.\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let commit = commit.trim();
    fs::write(repo.join("job guide.md"), "# Dirty worktree\n").unwrap();
    fs::write(repo.join("untracked.md"), "# Private Scratchpad\n").unwrap();

    let db = dir.path().join("index.sqlite");
    let source = dir.path().join("source");
    let repo = repo.to_str().unwrap();
    let source = source.to_str().unwrap();
    let db = db.to_str().unwrap();
    let export = [
        "export",
        "--repo",
        repo,
        "--source-dir",
        source,
        "--repo-url",
        REPO_URL,
    ];
    let ingest = ["ingest", "--source-dir", source, "--database", db];
    let search = ["query", "--database", db, "JOB003"];
    cli(&export);
    cli(&ingest);
    {
        let connection = rusqlite::Connection::open(db).unwrap();
        let auto_vacuum: i64 = connection
            .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
            .unwrap();
        assert_eq!(auto_vacuum, 1);
    }
    let result = cli(&search);
    let hit = &result["hits"][0];
    assert_eq!(hit["path"], "job guide.md");
    assert_eq!(
        hit["markdown"],
        "Set `dotnet_diagnostic.JOB003.severity = none` in .editorconfig.\n"
    );
    assert_eq!(hit["start_line"], 5);
    assert_eq!(hit["end_line"], 5);
    assert_eq!(
        hit["url"],
        format!("https://gitlab.example.org/docs/knowledge-base/-/blob/{commit}/job%20guide.md#L5")
    );
    assert_eq!(
        cli(&["query", "--database", db, "Scratchpad"])["status"],
        "no_match"
    );

    git(Path::new(repo), &["rm", "-f", "job guide.md"]);
    git(
        Path::new(repo),
        &["commit", "-m", "remove obsolete guidance"],
    );
    cli(&export);
    cli(&ingest);
    assert_eq!(cli(&search)["status"], "no_match");
}

#[test]
fn search_prefers_the_section_with_query_evidence_over_a_short_overview() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    let guide = "# HarborMQ - Queue Peek\n\nDiscover and peek at queue messages without draining them.\n\n## MCP Tools\n\nUse `harbormq_list` to discover queues and identify the virtual host.\n\n- `harbormq_peek_queue(env, vhost, queue, count?)` performs a non-destructive peek and requeues messages. The tool returns up to 10 messages with headers and payloads.\n\n## Typical workflow\n\nInspect the message headers and correlate them with application logs.\n";
    fs::write(repo.join("queues.md"), guide).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "document queue inspection"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let source = dir.path().join("source");
    let db = dir.path().join("index.sqlite");
    let db = db.to_str().unwrap();
    let source = source.to_str().unwrap();
    cli(&[
        "export",
        "--repo-url",
        REPO_URL,
        "--repo",
        repo.to_str().unwrap(),
        "--source-dir",
        source,
    ]);
    cli(&["ingest", "--source-dir", source, "--database", db]);

    for limit in ["1", "5"] {
        let result = cli(&[
            "query",
            "--database",
            db,
            "--limit",
            limit,
            "HarborMQ queue peek",
        ]);
        let hit = &result["hits"][0];
        assert_eq!(hit["section"], "HarborMQ - Queue Peek > MCP Tools");
        assert_eq!(hit["start_line"], 7);
        assert_eq!(hit["end_line"], 10);
        assert_eq!(
            hit["markdown"],
            guide
                .split_inclusive('\n')
                .skip(6)
                .take(4)
                .collect::<String>()
        );
        assert_eq!(
            hit["url"],
            format!(
                "https://gitlab.example.org/docs/knowledge-base/-/blob/{}/queues.md#L7-10",
                commit.trim()
            )
        );
    }
}

#[test]
fn search_combines_adjacent_sections_with_complementary_evidence() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    let guide = "# Registry settings\n\n## Cache precedence\n\nIn the cache, scoped overrides win over global defaults.\n\n### Environment filters\n\nWhen multiple environment filters match, the most specific filter wins.\n";
    fs::write(repo.join("registry.md"), guide).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "document override rules"]);
    let source = dir.path().join("source");
    let db = dir.path().join("index.sqlite");
    let db = db.to_str().unwrap();
    let source = source.to_str().unwrap();
    cli(&[
        "export",
        "--repo-url",
        REPO_URL,
        "--repo",
        repo.to_str().unwrap(),
        "--source-dir",
        source,
    ]);
    cli(&["ingest", "--source-dir", source, "--database", db]);
    let result = cli(&[
        "query",
        "--database",
        db,
        "cache precedence environment filters",
    ]);
    let hits = result["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];
    assert_eq!(hit["start_line"], 5);
    assert_eq!(hit["end_line"], 9);
    assert_eq!(
        hit["markdown"],
        guide.split_inclusive('\n').skip(4).collect::<String>()
    );
    assert!(hit["url"].as_str().unwrap().ends_with("/registry.md#L5-9"));
}

#[test]
fn managed_wiki_refresh_preserves_the_snapshot_on_failure_and_removes_deleted_pages() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("upstream");
    let wiki = dir.path().join("wiki");
    let mirror = dir.path().join("mirror");
    let source = dir.path().join("source");
    let db = dir.path().join("index.sqlite");
    for repository in [&repo, &wiki] {
        fs::create_dir(repository).unwrap();
        git(repository, &["init", "-b", "main"]);
    }
    fs::write(repo.join("guide.md"), "# Guide\n\nLegacyKBProbe.\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "primary"]);
    let primary = git(&repo, &["rev-parse", "HEAD"]);
    fs::create_dir(wiki.join("Runbooks")).unwrap();
    fs::write(
        wiki.join("Runbooks/Host alert.md"),
        "# Alert\n\nWikiAlertProbe is the documented check.\n",
    )
    .unwrap();
    fs::write(wiki.join("Scratch.md"), "# Scratch\n\nOutsideScopeProbe.\n").unwrap();
    git(&wiki, &["add", "."]);
    git(&wiki, &["commit", "-m", "wiki"]);
    // Construct the non-UTF-8 path in Git so the fixture also works on macOS filesystems.
    let mut entries = git(&wiki, &["ls-tree", "-z", "HEAD"]).into_bytes();
    let blob = git(&wiki, &["rev-parse", "HEAD:Scratch.md"]);
    entries.extend_from_slice(format!("100644 blob {}\t", blob.trim()).as_bytes());
    entries.extend_from_slice(b"scratch-\xff.md\0");
    let mut build_tree = git_command(&wiki)
        .args(["mktree", "-z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    build_tree
        .stdin
        .take()
        .unwrap()
        .write_all(&entries)
        .unwrap();
    let tree = build_tree.wait_with_output().unwrap();
    assert!(tree.status.success());
    let tree = String::from_utf8(tree.stdout).unwrap();
    let invalid_path_commit = git(
        &wiki,
        &[
            "commit-tree",
            tree.trim(),
            "-p",
            "HEAD",
            "-m",
            "outside allowlist",
        ],
    );
    git(
        &wiki,
        &["update-ref", "refs/heads/main", invalid_path_commit.trim()],
    );
    let wiki_commit = git(&wiki, &["rev-parse", "HEAD"]);
    let db = db.to_str().unwrap();
    let sync = || {
        let mut command = cli_command();
        command
            .args(["sync", "--repo-url", REPO_URL, "--mirror"])
            .arg(&mirror)
            .arg("--source-dir")
            .arg(&source)
            .arg("--remote")
            .arg(&repo)
            .env("GIT_CONFIG_COUNT", "1")
            .env(
                "GIT_CONFIG_KEY_0",
                format!("url.{}.insteadOf", wiki.display()),
            )
            .env(
                "GIT_CONFIG_VALUE_0",
                "git@gitlab.example.org:team/runbooks.wiki.git",
            );
        command
    };
    let ingest = || {
        cli(&[
            "ingest",
            "--source-dir",
            source.to_str().unwrap(),
            "--database",
            db,
        ])
    };
    command_output(&mut sync());
    let original = ingest();
    fs::write(
        dir.path().join("wiki-sources.json"),
        r#"[{"scope":"group","owner":"team/runbooks","paths":["Runbooks/"]}]"#,
    )
    .unwrap();
    command_output(&mut sync());
    let expanded = ingest();
    assert_eq!(expanded["files"], 2);
    assert_ne!(expanded["commit"], original["commit"]);
    let wiki_hit = &cli(&["query", "--database", db, "WikiAlertProbe"])["hits"][0];
    assert_eq!(wiki_hit["commit"], expanded["commit"]);
    assert_eq!(wiki_hit["source_commit"], wiki_commit.trim());
    assert_eq!(
        wiki_hit["path"],
        "gitlab-wikis/group/team/runbooks/Runbooks/Host alert.md"
    );
    assert_eq!(
        wiki_hit["url"],
        format!(
            "https://gitlab.example.org/groups/team/runbooks/-/wikis/Runbooks/Host%20alert?version_id={}",
            wiki_commit.trim()
        )
    );
    let kb_hit = &cli(&["query", "--database", db, "LegacyKBProbe"])["hits"][0];
    assert_eq!(
        kb_hit["url"],
        format!(
            "https://gitlab.example.org/docs/knowledge-base/-/blob/{}/guide.md#L3",
            primary.trim()
        )
    );
    assert_eq!(
        cli(&["query", "--database", db, "OutsideScopeProbe"])["status"],
        "no_match"
    );
    let before = fs::read(source.join("snapshot.json")).unwrap();
    let offline = dir.path().join("offline");
    fs::rename(&wiki, &offline).unwrap();
    assert!(!sync().output().unwrap().status.success());
    assert_eq!(before, fs::read(source.join("snapshot.json")).unwrap());
    fs::rename(&offline, &wiki).unwrap();
    git(&wiki, &["rm", "Runbooks/Host alert.md"]);
    git(&wiki, &["commit", "-m", "remove obsolete runbook"]);
    command_output(&mut sync());
    let refreshed = ingest();
    assert_eq!(refreshed["removed_files"], 1);
    assert_ne!(refreshed["commit"], expanded["commit"]);
    assert_eq!(
        cli(&["query", "--database", db, "WikiAlertProbe"])["status"],
        "no_match"
    );
}

#[test]
fn shortcut_import_preserves_document_title_and_uses_the_configured_workspace() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    fs::write(repo.join("guide.md"), "# Guide\n\nPrimaryKbEvidence.\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    let base = dir.path().join("base");
    let output = dir.path().join("source");
    let docs = dir.path().join("approved-docs.json");
    let db = dir.path().join("index.sqlite");
    cli(&[
        "export",
        "--repo-url",
        REPO_URL,
        "--repo",
        repo.to_str().unwrap(),
        "--source-dir",
        base.to_str().unwrap(),
    ]);
    let id = "c56387c1-d848-41b3-8b48-30a0dbe3975f";
    let mut page = json!({
        "id": id,
        "title": "Release verification",
        "updated_at": "2026-11-01T10:20:30Z",
        "content_markdown": "Verify ShortcutPilotEvidence before releasing.\n"
    });
    let import = [
        "import-shortcut",
        "--workspace",
        "example-team",
        "--base-dir",
        base.to_str().unwrap(),
        "--source-dir",
        output.to_str().unwrap(),
        "--docs",
        docs.to_str().unwrap(),
    ];
    fs::write(&docs, json!([page]).to_string()).unwrap();
    let imported = cli(&import);
    assert_eq!(imported["shortcut_docs"], 1);
    assert_eq!(cli(&import)["commit"], imported["commit"]);
    let ingest = [
        "ingest",
        "--source-dir",
        output.to_str().unwrap(),
        "--database",
        db.to_str().unwrap(),
    ];
    assert_eq!(cli(&ingest)["files"], 2);
    let query = |query| cli(&["query", "--database", db.to_str().unwrap(), query]);
    let results = query("ShortcutPilotEvidence");
    let hit = &results["hits"][0];
    assert_eq!(hit["title"], "Release verification");
    assert_eq!(hit["path"], format!("shortcut-docs/{id}.md"));
    assert_eq!(
        hit["url"],
        format!("https://api.app.shortcut.com/api/v4/example-team/documents/{id}")
    );
    assert!(hit["source_commit"].is_null());
    assert_eq!(hit["source_revision"].as_str().unwrap().len(), 64);
    assert_eq!(hit["commit"], imported["commit"]);
    assert_eq!(hit["start_line"], 1);
    assert_eq!(query("PrimaryKbEvidence")["status"], "ok");

    let mut other_workspace = import;
    other_workspace[2] = "another-team";
    let moved = cli(&other_workspace);
    assert_ne!(moved["commit"], imported["commit"]);
    assert_eq!(cli(&ingest)["updated_files"], 0);
    let moved_results = query("ShortcutPilotEvidence");
    let moved_hit = &moved_results["hits"][0];
    assert_eq!(
        moved_hit["url"],
        format!("https://api.app.shortcut.com/api/v4/another-team/documents/{id}")
    );
    assert_eq!(moved_hit["source_revision"], hit["source_revision"]);

    let snapshot_path = output.join("snapshot.json");
    let mut legacy: Value = serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
    legacy.as_object_mut().unwrap().remove("shortcut_workspace");
    fs::write(&snapshot_path, legacy.to_string()).unwrap();
    cli(&ingest);
    let legacy_results = query("ShortcutPilotEvidence");
    let legacy_hit = &legacy_results["hits"][0];
    assert_eq!(legacy_hit["url"], hit["path"]);
    assert_eq!(legacy_hit["source_revision"], hit["source_revision"]);
    assert!(legacy_hit["source_commit"].is_null());

    let before = fs::read(&snapshot_path).unwrap();
    fs::write(&docs, json!([page.clone(), page.clone()]).to_string()).unwrap();
    assert!(
        !cli_command()
            .args(import)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(fs::read(&snapshot_path).unwrap(), before);

    page["title"] = json!("Rollback verification");
    fs::write(&docs, json!([page]).to_string()).unwrap();
    assert_ne!(cli(&import)["commit"], imported["commit"]);
    assert_eq!(cli(&ingest)["updated_files"], 1);
    assert_eq!(
        query("ShortcutPilotEvidence")["hits"][0]["title"],
        "Rollback verification"
    );
    fs::write(&docs, "[]").unwrap();
    cli(&import);
    assert_eq!(cli(&ingest)["removed_files"], 1);
    assert_eq!(query("ShortcutPilotEvidence")["status"], "no_match");
}

#[test]
fn wiki_owner_keeps_the_relevant_section_in_the_search_results() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    let source = dir.path().join("source");
    let db = dir.path().join("index.sqlite");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    let mut markdown = String::from("## Worker throughput\n\n");
    markdown.push_str(
        "When worker throughput falls, the heap threshold is 4 GB before restarting.\n\n",
    );
    // Keep the dashboard links outside the adjacent-passage merge window.
    markdown.push_str("\n\n\n\n\n");
    for index in 1..=5 {
        markdown.push_str(&format!(
            "## Dashboard {index}\n\nOpen [telemetry](https://example.org/BeaconWorkerApi) for routine checks.\n\n"
        ));
    }
    fs::write(repo.join("runbook.md"), markdown).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "wiki runbook"]);
    let source = source.to_str().unwrap();
    let db = db.to_str().unwrap();
    cli(&[
        "export",
        "--repo-url",
        REPO_URL,
        "--repo",
        repo.to_str().unwrap(),
        "--source-dir",
        source,
    ]);
    let snapshot_path = Path::new(source).join("snapshot.json");
    let mut snapshot: Value = serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
    let files = snapshot["files"].take();
    snapshot["files"] = json!([]);
    snapshot["wikis"] = json!([{
        "scope": "project",
        "owner": "services/Example.BeaconWorkerApi",
        "commit": snapshot["commit"],
        "files": files,
    }]);
    fs::write(&snapshot_path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    cli(&["ingest", "--source-dir", source, "--database", db]);

    let result = cli(&[
        "query",
        "--database",
        db,
        "If worker throughput falls, what is the heap threshold for restarting BeaconWorkerApi?",
    ]);
    assert_eq!(result["status"], "ok");
    assert_eq!(
        result["hits"][0]["section"], "Worker throughput",
        "{result}"
    );
    assert!(
        result["hits"][0]["markdown"]
            .as_str()
            .unwrap()
            .contains("heap threshold is 4 GB")
    );
    let unsupported = cli(&[
        "query",
        "--database",
        db,
        "What is the Redis rate limit for BeaconWorkerApi?",
    ]);
    assert_eq!(unsupported["status"], "no_match");
}

#[test]
fn managed_sync_keeps_the_export_after_a_failed_fetch() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("upstream");
    let source = dir.path().join("source");
    let mirror = dir.path().join("mirror");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    fs::write(repo.join("guide.md"), "# Guidance\n\nPublished content.\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "published"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
    let args = [
        "sync",
        "--repo-url",
        REPO_URL,
        "--mirror",
        mirror.to_str().unwrap(),
        "--source-dir",
        source.to_str().unwrap(),
        "--remote",
        repo.to_str().unwrap(),
    ];
    let mut scheduled = args.to_vec();
    scheduled.extend(["--check-interval-seconds", "900"]);
    let first = cli(&scheduled);
    assert_eq!(first["upstream_commit"], commit);
    assert_eq!(first["managed"], true);
    assert_eq!(first["check_interval_seconds"], 900);
    let legacy_status: Value =
        serde_json::from_slice(&fs::read(source.join("sync.json")).unwrap()).unwrap();
    assert_eq!(
        legacy_status,
        json!({
            "managed": true,
            "upstream_commit": commit,
            "checked_at_unix": first["checked_at_unix"],
            "last_error": null,
        })
    );
    let before = fs::read(source.join("snapshot.json")).unwrap();
    assert_eq!(cli(&args)["check_interval_seconds"], 900);
    let stages = || -> Value {
        serde_json::from_slice(&fs::read(source.join("source-observations.json")).unwrap()).unwrap()
    };
    let unchanged = stages();
    assert_eq!(unchanged["check_interval_seconds"], 900);
    assert_eq!(unchanged["fetch"]["last_success"]["commit"], commit);
    assert_eq!(unchanged["export"]["last_success"]["code"], "unchanged");
    assert_eq!(unchanged["export"]["last_success"]["commit"], commit);

    let mut inline_interval = legacy_status;
    inline_interval["check_interval_seconds"] = json!(600);
    let mut old_stages = unchanged;
    old_stages
        .as_object_mut()
        .unwrap()
        .remove("check_interval_seconds");
    fs::write(source.join("sync.json"), inline_interval.to_string()).unwrap();
    fs::write(
        source.join("source-observations.json"),
        old_stages.to_string(),
    )
    .unwrap();
    assert_eq!(cli(&args)["check_interval_seconds"], 600);
    assert_eq!(stages()["check_interval_seconds"], 600);
    let migrated: Value =
        serde_json::from_slice(&fs::read(source.join("sync.json")).unwrap()).unwrap();
    assert!(migrated.get("check_interval_seconds").is_none());

    fs::rename(&repo, dir.path().join("unavailable")).unwrap();
    let retry = cli_command().args(args).output().unwrap();
    assert!(!retry.status.success());
    let status: Value =
        serde_json::from_slice(&fs::read(source.join("sync.json")).unwrap()).unwrap();
    assert_eq!(status["last_error"], "git_remote_unavailable");
    assert_eq!(status["upstream_commit"], commit);
    assert_eq!(fs::read(source.join("snapshot.json")).unwrap(), before);
    let failed = stages();
    assert_eq!(failed["fetch"]["last_attempt"]["code"], "fetch_failed");
    assert_eq!(failed["fetch"]["last_attempt"]["outcome"], "failed");
    assert_eq!(failed["fetch"]["last_success"]["commit"], commit);

    let ssh = dir.path().join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nprintf '%s\\n' 'Host key verification failed.' 'private-fixture-stderr' >&2\nexit 255\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let inherited_path = std::env::var_os("PATH").unwrap();
    let path = std::env::join_paths(
        std::iter::once(dir.path().to_path_buf()).chain(std::env::split_paths(&inherited_path)),
    )
    .unwrap();
    let failed_ssh = cli_command()
        .args(&args[..args.len() - 1])
        .arg("ssh://fixture@fixture.invalid/repo")
        .env("GIT_SSH_VARIANT", "ssh")
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(!failed_ssh.status.success());
    let diagnostics = String::from_utf8(failed_ssh.stderr).unwrap();
    assert!(diagnostics.contains("SSH host key verification failed"));
    assert!(!diagnostics.contains("private-fixture-stderr"));
    let status = fs::read_to_string(source.join("sync.json")).unwrap();
    assert!(!status.contains("private-fixture-stderr"));
    let status: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(status["last_error"], "git_host_key_failed");
    assert_eq!(fs::read(source.join("snapshot.json")).unwrap(), before);
}

#[test]
fn search_normalizes_queries_and_requires_the_requested_subject() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    for (path, text) in [
        (
            "network.md",
            "---\nlabel: PrivateMetadataMarker\n---\n# Networking\n\nKestrel responds to ECONNRESET when the peer closes the connection.\n",
        ),
        (
            "deploy.md",
            "# Deploys\n\nConfigure a deploy with the gateway service route. Set the deploy timeout to five minutes.\n",
        ),
        (
            "widget.md",
            "# WidgetService\n\nWidgetService creates order widgets. Call `widget_lookup` to fetch one.\n",
        ),
    ] {
        fs::write(repo.join(path), text).unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    let source = dir.path().join("source");
    let db = dir.path().join("index.sqlite");
    let source = source.to_str().unwrap();
    let db = db.to_str().unwrap();
    cli(&[
        "export",
        "--repo-url",
        REPO_URL,
        "--repo",
        repo.to_str().unwrap(),
        "--source-dir",
        source,
    ]);
    let ingest = ["ingest", "--source-dir", source, "--database", db];
    assert_eq!(cli(&ingest)["updated_files"], 3);
    assert_eq!(cli(&ingest)["updated_files"], 0);
    rusqlite::Connection::open(db)
        .unwrap()
        .execute("UPDATE kb_revision SET passage_version=0", [])
        .unwrap();
    assert_eq!(cli(&ingest)["updated_files"], 3);
    let search = |query| cli(&["query", "--database", db, query]);
    for (query, path) in [
        ("Why do I get ECONNRESET?", "network.md"),
        ("Tell me about kestrel", "network.md"),
        ("configuring deploys", "deploy.md"),
        ("gateway services routes", "deploy.md"),
        ("widgetLookup", "widget.md"),
    ] {
        assert_eq!(search(query)["hits"][0]["path"], path, "{query}");
    }
    assert_eq!(search("Kestrel")["hits"][0]["start_line"], 6);
    assert_eq!(
        search("PrivateMetadataMarker")["reason"],
        "no_ranked_matches"
    );
    assert_eq!(
        search("Timeout for WidgetService")["reason"],
        "insufficient_evidence"
    );
}

#[test]
fn export_skips_unusable_markdown_and_publishes_valid_files() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    fs::write(
        repo.join("guide.md"),
        "# Valid\n\nUse the only valid file.\n",
    )
    .unwrap();
    fs::write(repo.join("long-line.md"), "x".repeat(8 * 1024)).unwrap();
    fs::write(repo.join("large.md"), "x\n".repeat(300 * 1024)).unwrap();
    fs::write(repo.join("broken.md"), [0xff]).unwrap();
    std::os::unix::fs::symlink("guide.md", repo.join("symlink.md")).unwrap();
    fs::create_dir(repo.join("gitlab-wikis")).unwrap();
    fs::write(repo.join("gitlab-wikis/collision.md"), "# reserved").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    let source = dir.path().join("source");
    let export = cli(&[
        "export",
        "--repo-url",
        REPO_URL,
        "--repo",
        repo.to_str().unwrap(),
        "--source-dir",
        source.to_str().unwrap(),
    ]);
    assert_eq!(export["files"], 1);
    let snapshot: Value =
        serde_json::from_slice(&fs::read(source.join("snapshot.json")).unwrap()).unwrap();
    assert_eq!(snapshot["files"][0]["path"], "guide.md");
}

#[test]
fn search_checks_short_endpoint_evidence_and_explicit_automation() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    fs::write(
        repo.join("vendor-api.md"),
        "# Vendor API documentation\n\nStage URL for publishing Markdown: <https://stage.example.org/swagger/index.html#/Docs/Docs_PublishFile>\n",
    )
    .unwrap();
    fs::write(
        repo.join("release.md"),
        "# Release ticket states\n\nThe release ticket moves to Implementing once development begins.\n",
    )
    .unwrap();
    fs::write(
        repo.join("subscriptions.md"),
        "# Shortcut webhooks\n\nUse a Shortcut webhook to notify the audit team after deployment.\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    let source = dir.path().join("source");
    let db = dir.path().join("index.sqlite");
    let source = source.to_str().unwrap();
    let db = db.to_str().unwrap();
    cli(&[
        "export",
        "--repo-url",
        REPO_URL,
        "--repo",
        repo.to_str().unwrap(),
        "--source-dir",
        source,
    ]);
    cli(&["ingest", "--source-dir", source, "--database", db]);

    let search = |query| cli(&["query", "--database", db, query]);
    let route =
        search("Which staging Swagger operation publishes Markdown for Vendor API documentation?");
    let automation = search(
        "Which scheduled Shortcut webhook automatically moves release tickets to Implementing?",
    );
    let documented_webhook =
        search("Which Shortcut webhook notifies the audit team after deployment?");
    let production = search(
        "Which production Swagger operation publishes Markdown for Vendor API documentation?",
    );
    assert_eq!(
        route["status"], "ok",
        "route={route}, automation={automation}"
    );
    assert!(
        route["hits"][0]["markdown"]
            .as_str()
            .unwrap()
            .contains("Docs_PublishFile")
    );
    assert_eq!(automation["status"], "no_match", "{automation}");
    assert_eq!(documented_webhook["status"], "ok", "{documented_webhook}");
    assert_eq!(production["status"], "no_match", "{production}");
}

#[test]
fn search_prefers_the_section_that_performs_the_requested_action() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    fs::write(
        repo.join("invite.md"),
        "# Widget invitation lifecycle\n\n## Grant account access\n\nAn account owner can send a pending widget delegate invitation and revoke it before it is accepted.\n\n\n\n\n\n\n## Account owner revokes a pending invitation\n\nOpen Pending Invitations, select the delegate, and click Revoke Invite.\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    let source = dir.path().join("source");
    let db = dir.path().join("index.sqlite");
    let source = source.to_str().unwrap();
    let db = db.to_str().unwrap();
    cli(&[
        "export",
        "--repo-url",
        REPO_URL,
        "--repo",
        repo.to_str().unwrap(),
        "--source-dir",
        source,
    ]);
    cli(&["ingest", "--source-dir", source, "--database", db]);
    let result = cli(&[
        "query",
        "--database",
        db,
        "Where does a Widget account owner revoke a pending delegate invitation before it is accepted?",
    ]);
    assert_eq!(
        result["hits"][0]["section"],
        "Widget invitation lifecycle > Account owner revokes a pending invitation",
        "{result}"
    );
}

#[test]
fn search_requires_evidence_for_the_requested_property_and_explicit_reference() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    fs::write(
        repo.join("ci.md"),
        "# GitLab CI templates\n\n## Shared configuration\n\nThe shared GitLab CI templates produce build artifacts and use the main ref. Ticket 4321 changed the ref ([sc-4321](https://example.org/4321)).\n\n## Artifact downloads\n\nJobs can download build artifacts from earlier stages.\n",
    ).unwrap();
    fs::write(
        repo.join("cache.md"),
        "# Redis cache configuration\n\nThe Redis cache lease duration defaults to 12 minutes.\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    let source = dir.path().join("source");
    let db = dir.path().join("index.sqlite");
    let source = source.to_str().unwrap();
    let db = db.to_str().unwrap();
    cli(&[
        "export",
        "--repo-url",
        REPO_URL,
        "--repo",
        repo.to_str().unwrap(),
        "--source-dir",
        source,
    ]);
    cli(&["ingest", "--source-dir", source, "--database", db]);
    let search = |query| cli(&["query", "--database", db, query]);

    let no_match =
        search("What is the default artifact retention period for the shared GitLab CI templates?");
    assert_eq!(no_match["status"], "no_match");
    assert_eq!(no_match["reason"], "insufficient_evidence");
    assert_eq!(no_match["hits"], json!([]));
    assert_eq!(
        search("Which GitLab CI templates changed in ticket 8765?")["status"],
        "no_match"
    );
    assert_eq!(
        search("Which GitLab CI templates changed in ticket 4321?")["status"],
        "ok"
    );
    assert_eq!(search("GitLab CI templates SC-4321")["status"], "ok");
    assert_eq!(search("GitLab CI templates sc-8765")["status"], "no_match");
    let documented = search("What is the default lease duration for the Redis cache?");
    assert_eq!(documented["status"], "ok");
    assert!(
        documented["hits"][0]["markdown"]
            .as_str()
            .unwrap()
            .contains("12 minutes")
    );
}

#[test]
fn ops_relay_copies_the_producers_report_and_preserves_it_on_failure() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("producer.json");
    let docker = dir.path().join("docker");
    let inbox = dir.path().join("inbox");
    let mut report = json!({
        "schema_version": 1, "service": "know-things", "component": "daemon",
        "instance_id": "ad5b8617-524d-4916-ab00-80e4affc4297", "sampled_at_unix_ms": 1795453200000_u64,
        "health": {"state":"ready","freshness":"unknown"},
        "usage": {
            "kb_search":{"ok":1,"rejected":0,"error":0,"cancelled":0,"duration_ms_total":1},
            "kb_read":{"ok":0,"rejected":0,"error":0,"cancelled":0,"duration_ms_total":0}
        },
        "details":{"files":33,"passages":378,"source_mode":"snapshot"}
    });
    let original = serde_json::to_vec(&report).unwrap();
    fs::write(&source, &original).unwrap();
    fs::write(
        &docker,
        "#!/bin/sh\nexec /bin/cat \"$(dirname \"$0\")/producer.json\"\n",
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
    let args = [
        "relay-ops",
        "--docker",
        docker.to_str().unwrap(),
        "--inbox",
        inbox.to_str().unwrap(),
    ];
    let launch = || cli_command().args(args).output().unwrap();
    let status = launch();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let destination = inbox.join("know-things-daemon.json");
    assert_eq!(fs::read(&destination).unwrap(), original);
    assert_eq!(
        inbox.metadata().unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        destination.metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );

    report["service"] = json!("know-code");
    let wrong_service = serde_json::to_vec(&report).unwrap();
    report["service"] = json!("know-things");
    report["schema_version"] = json!(2);
    for invalid in [
        wrong_service,
        serde_json::to_vec(&report).unwrap(),
        b"{".to_vec(),
        vec![b' '; 16_385],
    ] {
        fs::write(&source, invalid).unwrap();
        assert!(!launch().status.success());
        assert_eq!(fs::read(&destination).unwrap(), original);
    }
}

struct McpSession {
    process: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    id: u64,
}

impl McpSession {
    async fn start(socket: &str) -> Self {
        let mut process = AsyncCommand::new(env!("CARGO_BIN_EXE_know-things"))
            .args(["mcp", "--socket", socket])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = process.stdin.take().unwrap();
        let output = BufReader::new(process.stdout.take().unwrap());
        let mut session = Self {
            process,
            input,
            output,
            id: 0,
        };
        let result = session
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "integration-test", "version": "1"},
                }),
            )
            .await;
        assert_eq!(result["serverInfo"]["name"], "know-things");
        session
            .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        session
    }

    async fn send(&mut self, value: Value) {
        self.input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params}))
            .await;
        let mut line = String::new();
        let bytes = timeout(Duration::from_secs(4), self.output.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(bytes > 0, "{method} returned EOF");
        let answer: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(answer["id"], self.id, "{answer}");
        assert!(answer["error"].is_null(), "{answer}");
        answer["result"].clone()
    }

    async fn tool(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self
            .request("tools/call", json!({"name": tool, "arguments": arguments}))
            .await;
        assert!(result["structuredContent"].is_object(), "{result}");
        result["structuredContent"].clone()
    }

    async fn close(mut self) {
        drop(self.input);
        assert!(
            timeout(Duration::from_secs(4), self.process.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

async fn wait_status(socket: &str, matches: impl Fn(&Value) -> bool) -> Value {
    let start = Instant::now();
    loop {
        let output = cli_command()
            .args(["status", "--socket", socket])
            .output()
            .unwrap();
        if output.status.success() {
            let value = serde_json::from_slice(&output.stdout).unwrap();
            if matches(&value) {
                return value;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "daemon state did not change"
        );
        sleep(Duration::from_millis(30)).await;
    }
}

async fn wait_report(path: &Path, matches: impl Fn(&Value) -> bool) -> Value {
    let start = Instant::now();
    loop {
        if let Ok(bytes) = fs::read(path) {
            let value = serde_json::from_slice(&bytes).expect("report must be atomically readable");
            if matches(&value) {
                return value;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(35),
            "Know Ops report did not change"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn malformed_sync_status_is_visible_and_recovery_keeps_the_index_available() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    fs::write(repo.join("guide.md"), "# Guide\n\nUse `RefreshProbe`.\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    let source = dir.path().join("source");
    let mirror = dir.path().join("mirror");
    let db = dir.path().join("index.sqlite");
    let socket = dir.path().join("kb.sock");
    let socket = socket.to_str().unwrap();
    let fresh = cli(&[
        "sync",
        "--repo-url",
        REPO_URL,
        "--mirror",
        mirror.to_str().unwrap(),
        "--source-dir",
        source.to_str().unwrap(),
        "--remote",
        repo.to_str().unwrap(),
        "--check-interval-seconds",
        "900",
    ]);
    let mut daemon = AsyncCommand::new(env!("CARGO_BIN_EXE_know-things"))
        .args([
            "serve",
            "--source-dir",
            source.to_str().unwrap(),
            "--database",
            db.to_str().unwrap(),
            "--socket",
            socket,
            "--poll-seconds",
            "1",
        ])
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let commit = fresh["upstream_commit"].clone();
    let current = wait_status(socket, |status| status["freshness"]["status"] == "current").await;
    assert_eq!(current["freshness"]["check_interval_seconds"], 900);

    let sync_file = source.join("sync.json");
    let temporary = source.join("update-sync.json");
    fs::write(&temporary, br#"{"managed":"private-parse-input""#).unwrap();
    fs::rename(&temporary, &sync_file).unwrap();
    let degraded = wait_status(socket, |status| {
        status["freshness"]["last_sync_error"] == "sync_status_unreadable"
    })
    .await;
    assert_eq!(degraded["freshness"]["status"], "stale");
    assert_eq!(degraded["freshness"]["source_mode"], "managed");
    assert_eq!(degraded["indexed_commit"], commit);
    assert_eq!(
        cli(&["search", "--socket", socket, "RefreshProbe"])["hits"][0]["path"],
        "guide.md"
    );

    let mut published = fresh;
    published
        .as_object_mut()
        .unwrap()
        .remove("check_interval_seconds");
    published["future_host_field"] = json!({"added": "metadata"});
    fs::write(&temporary, published.to_string()).unwrap();
    fs::rename(&temporary, &sync_file).unwrap();
    let recovered = wait_status(socket, |status| status["freshness"]["status"] == "current").await;
    assert!(recovered["freshness"]["last_sync_error"].is_null());
    assert_eq!(recovered["indexed_commit"], commit);
    assert_eq!(recovered["freshness"]["check_interval_seconds"], 900);
    daemon.kill().await.unwrap();
    let output = daemon.wait_with_output().await.unwrap();
    let logs = String::from_utf8(output.stderr).unwrap();
    assert!(logs.contains("KB could not read sync.json"), "{logs}");
    assert!(!logs.contains("private-parse-input"), "{logs}");
}

#[tokio::test]
async fn stdio_sessions_share_a_daemon_and_failed_refresh_preserves_the_revision() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    fs::write(
        repo.join("guide.md"),
        "# Samples\n\n## Environment\n\nUse `OwnerService` for ownership.\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
    let source = dir.path().join("source");
    let socket = dir.path().join("kb.sock");
    let db = dir.path().join("index.sqlite");
    let socket = socket.to_str().unwrap();
    let export = [
        "export",
        "--repo-url",
        REPO_URL,
        "--repo",
        repo.to_str().unwrap(),
        "--source-dir",
        source.to_str().unwrap(),
    ];
    let args = [
        "serve",
        "--source-dir",
        source.to_str().unwrap(),
        "--database",
        db.to_str().unwrap(),
        "--socket",
        socket,
        "--poll-seconds",
        "1",
    ];
    let mut daemon = AsyncCommand::new(env!("CARGO_BIN_EXE_know-things"))
        .args(args)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let report_path = dir.path().join("ops-snapshot.json");
    let startup = wait_report(&report_path, |v| v["health"]["state"] == "starting").await;
    assert_eq!(startup["health"]["reason"], "waiting_for_index");
    assert_eq!(startup["usage"]["kb_search"]["ok"], 0);
    assert_eq!(startup["usage"]["kb_read"]["ok"], 0);
    wait_status(socket, |v| v["freshness"]["status"] == "unavailable").await;
    cli(&export);
    wait_status(socket, |v| v["indexed_commit"] == commit).await;

    let admin = dir.path().join("admin.sock");
    let admin = admin.to_str().unwrap();
    let read_admin = || cli(&["admin", "status", "--socket", admin]);
    let ready = read_admin();
    assert_eq!(ready["source_mode"], "snapshot");
    assert_eq!(ready["health"]["state"], "ready");
    assert_eq!(ready["health"]["freshness"], "unknown");
    assert!(ready["upstream_commit"].is_null());
    assert_eq!(ready["index_readable"], true);
    assert_eq!(ready["exported_commit"], commit);
    assert_eq!(ready["indexed_commit"], commit);
    assert_eq!(ready["stages"]["export"]["last_success"]["commit"], commit);
    assert_eq!(ready["stages"]["index"]["last_success"]["updated_files"], 1);

    let (mut first, mut second) =
        tokio::join!(McpSession::start(socket), McpSession::start(socket));
    let oversized = json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "tools/call",
        "params": {"name": "kb_search", "arguments": {"query": "x".repeat(65_536)}}
    });
    second
        .input
        .write_all(format!("{oversized}\n").as_bytes())
        .await
        .unwrap();
    let mut rejection = String::new();
    timeout(
        Duration::from_secs(4),
        second.output.read_line(&mut rejection),
    )
    .await
    .unwrap()
    .unwrap();
    let rejection: Value = serde_json::from_str(&rejection).unwrap();
    assert_eq!(rejection["id"], 99);
    assert_eq!(rejection["error"]["code"], -32600);
    assert!(
        rejection["error"]["message"]
            .as_str()
            .unwrap()
            .contains("64 KiB")
    );
    let list = first.request("tools/list", json!({})).await;
    assert_eq!(list["tools"].as_array().unwrap().len(), 3);
    let (search, status) = tokio::join!(
        first.tool("kb_search", json!({"query": "OwnerService"})),
        second.tool("kb_status", json!({})),
    );
    assert_eq!(search["hits"][0]["section"], "Samples > Environment");
    assert_eq!(status["indexed_commit"], commit);
    let unsupported = first
        .tool(
            "kb_search",
            json!({"query": "What is the OwnerService retention period for ownership records?"}),
        )
        .await;
    assert_eq!(unsupported["status"], "no_match");
    assert_eq!(unsupported["reason"], "insufficient_evidence");
    assert_eq!(unsupported["hits"], json!([]));
    first.close().await;
    let read = second
        .tool(
            "kb_read",
            json!({
                "path": "guide.md", "commit": commit, "start_line": 5, "end_line": 9
            }),
        )
        .await;
    assert_eq!(
        read["evidence"]["markdown"],
        "Use `OwnerService` for ownership.\n"
    );
    assert_eq!(read["truncated"], true);

    let malformed = dir.path().join("bad.json");
    fs::write(&malformed, "{").unwrap();
    fs::rename(&malformed, source.join("snapshot.json")).unwrap();
    wait_status(socket, |v| v["freshness"]["status"] == "stale").await;
    let stale = second
        .tool("kb_search", json!({"query": "OwnerService"}))
        .await;
    assert_eq!(stale["freshness"]["last_index_error"], "invalid_snapshot");
    assert_eq!(stale["hits"][0]["commit"], commit);
    let degraded_status = read_admin();
    assert_eq!(degraded_status["health"]["state"], "degraded");
    assert_eq!(degraded_status["indexed_commit"], commit);
    assert_eq!(
        degraded_status["stages"]["index"]["last_attempt"]["code"],
        "invalid_snapshot"
    );
    assert_eq!(
        degraded_status["stages"]["index"]["last_success"]["commit"],
        commit
    );
    let invalid = second
        .tool("kb_search", json!({"query":"OwnerService","limit":0}))
        .await;
    assert_eq!(invalid["code"], "invalid_arguments");
    let degraded = wait_report(&report_path, |v| v["health"]["state"] == "degraded").await;
    assert_eq!(
        degraded["health"],
        json!({"state":"degraded","freshness":"stale","reason":"index_error"})
    );
    assert_eq!(degraded["details"]["files"], 1);
    assert_eq!(degraded["details"]["source_mode"], "snapshot");
    assert_eq!(degraded["usage"]["kb_search"]["ok"], 3);
    assert_eq!(degraded["usage"]["kb_search"]["rejected"], 1);
    assert_eq!(degraded["usage"]["kb_read"]["ok"], 1);
    assert!(degraded["usage"].get("kb_status").is_none());

    fs::write(
        repo.join("guide.md"),
        "# Samples\n\nUse `NewOwnerService`.\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "update"]);
    cli(&export);
    wait_status(socket, |v| {
        v["indexed_commit"].is_string() && v["indexed_commit"] != commit
    })
    .await;
    let old = second
        .tool(
            "kb_read",
            json!({
                "path": "guide.md", "commit": commit, "start_line": 5, "end_line": 5
            }),
        )
        .await;
    assert_eq!(old["status"], "stale_revision");
    assert_eq!(
        second
            .tool("kb_search", json!({"query": "OwnerService"}))
            .await["status"],
        "no_match"
    );
    second.close().await;

    let metrics = read_admin()["metrics"].clone();
    assert_eq!(metrics["search"], json!({"hits":2,"empty":0,"abstained":2}));
    assert_eq!(metrics["read"], json!({"stale_revision":1,"truncated":1}));
    let latency = &metrics["latency_us"];
    assert_eq!(
        latency["upper_bounds"],
        json!([
            250, 500, 1000, 2000, 5000, 10000, 25000, 50000, 100000, 500000, 1000000, 2000000
        ])
    );
    for (operation, count) in [("kb_search", 5), ("kb_read", 2)] {
        let buckets = latency[operation].as_array().unwrap();
        assert_eq!(buckets.len(), 13);
        assert_eq!(
            buckets.iter().map(|n| n.as_u64().unwrap()).sum::<u64>(),
            count
        );
    }
    assert!(degraded.get("metrics").is_none());

    daemon.kill().await.unwrap();
    let mut restarted = AsyncCommand::new(env!("CARGO_BIN_EXE_know-things"))
        .args(args)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_status(socket, |v| v["files"] == 1).await;
    let restarted_report =
        wait_report(&report_path, |v| v["instance_id"] != startup["instance_id"]).await;
    assert_eq!(restarted_report["usage"]["kb_search"]["ok"], 0);
    assert_eq!(restarted_report["usage"]["kb_read"]["ok"], 0);
    let reset = read_admin();
    assert_ne!(reset["instance_id"], ready["instance_id"]);
    assert_eq!(
        reset["metrics"]["search"],
        json!({"hits":0,"empty":0,"abstained":0})
    );
    let questions = dir.path().join("questions.jsonl");
    let answerable = json!({
        "id": "ownership",
        "query": "NewOwnerService",
        "evidence_sets": [[{"path": "guide.md", "start_line": 3, "end_line": 3}]]
    });
    let unsupported = json!({
        "id": "unknown",
        "query": "UnknownService",
        "evidence_sets": []
    });
    fs::write(&questions, format!("{answerable}\n{unsupported}\n")).unwrap();
    let evaluate = [
        "evaluate",
        "--socket",
        socket,
        "--questions",
        questions.to_str().unwrap(),
        "--repetitions",
        "1",
        "--commit",
    ];
    let current = reset["indexed_commit"].as_str().unwrap();
    let score = cli(&[evaluate.as_slice(), &[current]].concat());
    assert_eq!(score["questions"], 2);
    assert_eq!(score["complete_at_five"], 1);
    assert_eq!(score["unsupported_queries_with_hits"], 0);
    assert_eq!(score["latency"]["samples"], 2);
    let wrong_revision = cli_command().args(evaluate).arg(commit).output().unwrap();
    assert!(!wrong_revision.status.success());
    assert!(String::from_utf8_lossy(&wrong_revision.stderr).contains("index revision changed"));
    restarted.kill().await.unwrap();
}

fn console_profile(dir: &Path, docker: &Path, mode: Value) -> std::path::PathBuf {
    let config = dir.join("console.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "docker": { "executable": docker, "context": "isolated-test", "container": "isolated-kb" },
            "state_dir": dir.join("operations"),
            "relay_inbox": dir.join("inbox"),
            "source": mode
        }))
        .unwrap(),
    )
    .unwrap();
    config
}

#[test]
fn console_diagnose_distinguishes_docker_and_daemon_states() {
    let dir = tempdir().unwrap();
    let marker = dir.path().join("docker-state");
    let docker = dir.path().join("docker");
    fs::write(
        &docker,
        r#"#!/bin/sh
set -eu
base="$(dirname "$0")"
state="$(cat "$base/docker-state")"
[ "$1" = --context ] && [ "$2" = isolated-test ]
shift 2
case "$1:$2:$state" in
  container:inspect:offline) echo 'daemon unavailable' >&2; exit 1 ;;
  container:inspect:missing) echo 'Error response from daemon: No such container: isolated-kb' >&2; exit 1 ;;
  container:inspect:stopped) echo '{"Running":false}';;
  container:inspect:*) echo '{"Running":true}';;
  info:*) [ "$state" != offline ] && echo '"1.0"';;
  exec:--:old)
    if [ "$5" = admin ]; then exit 2; fi
    echo '{"indexed_commit":"known"}'
    ;;
  exec:--:disconnected) exit 1 ;;
  *) exit 90 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
    let config = console_profile(
        dir.path(),
        &docker,
        json!({
            "mode":"snapshot",
            "checkout":dir.path().join("repo"),
            "git_ref":"HEAD",
            "repo_url":"https://gitlab.example.org/docs/knowledge-base",
            "source_dir":dir.path().join("source")
        }),
    );
    for (fixture, container, daemon) in [
        ("offline", "docker_unreachable", "unknown"),
        ("missing", "missing", "unknown"),
        ("stopped", "stopped", "unknown"),
        ("old", "running", "unsupported"),
        ("disconnected", "running", "unreachable"),
    ] {
        fs::write(&marker, fixture).unwrap();
        let result = cli(&["ops", "diagnose", "--config", config.to_str().unwrap()]);
        assert_eq!(result["schema_version"], 1);
        assert_eq!(result["source_mode"], "snapshot");
        assert_eq!(result["container"]["state"], container, "{result}");
        assert_eq!(result["daemon"]["state"], daemon, "{result}");
        assert!(result["daemon"]["sample"].is_null());
        assert_eq!(result["relay"]["state"], "missing");
    }
    let inbox = dir.path().join("inbox");
    fs::create_dir(&inbox).unwrap();
    let report = json!({
        "schema_version":1, "service":"know-things", "component":"daemon",
        "instance_id":"ad5b8617-524d-4916-ab00-80e4affc4297",
        "sampled_at_unix_ms": 9_000_000_000_000_u64,
        "health":{"state":"ready","freshness":"unknown"},
        "usage":{
            "kb_search":{"ok":0,"rejected":0,"error":0,"cancelled":0,"duration_ms_total":0},
            "kb_read":{"ok":0,"rejected":0,"error":0,"cancelled":0,"duration_ms_total":0}
        }
    });
    fs::write(inbox.join("know-things-daemon.json"), report.to_string()).unwrap();
    let result = cli(&["ops", "diagnose", "--config", config.to_str().unwrap()]);
    assert_eq!(result["relay"]["state"], "invalid");
    assert!(result["relay"]["sampled_at_unix_ms"].is_null());
}

#[tokio::test]
async fn console_actions_complete_only_after_the_requested_revision_is_searchable() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    let source = dir.path().join("source");
    let socket = dir.path().join("kb.sock");
    let admin = dir.path().join("admin.sock");
    let database = dir.path().join("index.sqlite");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    let commit = |content: &str| {
        fs::write(
            repo.join("guide.md"),
            format!("# Guidance\n\nUse `{content}`.\n"),
        )
        .unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", content]);
        git(&repo, &["rev-parse", "HEAD"]).trim().to_owned()
    };
    let before = commit("InitialThing");
    let mut daemon = AsyncCommand::new(env!("CARGO_BIN_EXE_know-things"))
        .arg("serve")
        .arg("--source-dir")
        .arg(&source)
        .arg("--database")
        .arg(&database)
        .arg("--socket")
        .arg(&socket)
        .args(["--poll-seconds", "3600"])
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let socket = socket.to_str().unwrap();
    wait_status(socket, |value| value["indexed_commit"].is_null()).await;

    let docker = dir.path().join("docker");
    fs::write(
        &docker,
        r#"#!/bin/sh
set -eu
base="$(dirname "$0")"
[ "$1" = --context ] && [ "$2" = isolated-test ]
shift 2
if [ "$1" = container ] && [ "$2" = inspect ]; then
  echo '{"Running":true}'
  exit 0
fi
[ "$1" = exec ] && [ "$2" = -- ] && [ "$3" = isolated-kb ]
shift 4
if [ "$1" = admin ] && [ "$2" = operation ] && [ -f "$base/hold-operation" ]; then
  : > "$base/waiting-operation"
  exec /bin/sleep 5
fi
exec "$KNOW_THINGS_FIXTURE_BINARY" "$@" --socket "$base/admin.sock"
"#,
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
    let config = console_profile(
        dir.path(),
        &docker,
        json!({
            "mode": "snapshot", "checkout": repo, "git_ref": "HEAD",
            "repo_url": "https://gitlab.example.org/docs/knowledge-base", "source_dir": source
        }),
    );
    let host = |args: &[&str]| -> (bool, Value) {
        let output = cli_command()
            .env(
                "KNOW_THINGS_FIXTURE_BINARY",
                env!("CARGO_BIN_EXE_know-things"),
            )
            .args(["ops", args[0], "--config"])
            .arg(&config)
            .args(&args[1..])
            .output()
            .unwrap();
        let json = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid console JSON {error}: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.success(), json)
    };
    let action = |command, id: &str| host(&[command, "--operation-id", id]);
    let initial = uuid::Uuid::new_v4().to_string();
    let (success, first) = action("refresh", &initial);
    assert!(success, "{first}");
    assert_eq!(first["phase"], "done");
    assert_eq!(first["state"], "succeeded");
    assert_eq!(first["target_commit"], before);
    assert_eq!(first["indexed_commit"], before);
    assert_eq!(
        cli(&["search", "--socket", socket, "InitialThing"])["status"],
        "ok"
    );

    let after = commit("UpdatedThing");
    assert_eq!(
        action("refresh", &initial).1,
        first,
        "retries replay the same ID"
    );
    assert_eq!(action("operation", &initial).1, first);
    fs::write(source.join("malformed"), "{").unwrap();
    fs::rename(source.join("malformed"), source.join("snapshot.json")).unwrap();
    let index_attempt = || {
        host(&["diagnose"]).1["daemon"]["sample"]["stages"]["index"]["last_attempt"]["started_at_unix_ms"].as_u64().unwrap()
    };

    let (success, failed) = action("retry-index", &uuid::Uuid::new_v4().to_string());
    assert!(!success, "{failed}");
    assert_eq!(failed["code"], "invalid_snapshot");
    assert_eq!(failed["indexed_commit"], before);
    let first_attempt = index_attempt();
    sleep(Duration::from_millis(10)).await;
    let (success, failed_again) = action("retry-index", &uuid::Uuid::new_v4().to_string());
    assert!(!success, "{failed_again}");
    assert_eq!(failed_again["code"], "invalid_snapshot");
    assert!(
        index_attempt() > first_attempt,
        "retry revalidated the same file stamp"
    );
    assert_eq!(
        cli(&["search", "--socket", socket, "InitialThing"])["status"],
        "ok"
    );

    cli(&[
        "export",
        "--repo-url",
        REPO_URL,
        "--repo",
        repo.to_str().unwrap(),
        "--source-dir",
        source.to_str().unwrap(),
    ]);
    let admin = admin.to_str().unwrap();
    let superseded = uuid::Uuid::new_v4().to_string();
    cli(&[
        "admin",
        "refresh",
        "--socket",
        admin,
        "--operation-id",
        &superseded,
        "--expected-commit",
        &before,
    ]);
    let result = cli(&[
        "admin",
        "operation",
        "--socket",
        admin,
        "--operation-id",
        &superseded,
    ]);
    assert_eq!(result["code"], "snapshot_superseded");
    assert_eq!(result["indexed_commit"], before);

    let (success, recovery) = action("refresh", &uuid::Uuid::new_v4().to_string());
    assert!(success, "{recovery}");
    assert_eq!(recovery["indexed_commit"], after);
    assert_eq!(
        cli(&["search", "--socket", socket, "UpdatedThing"])["status"],
        "ok"
    );
    let (success, unchanged) = action("retry-index", &uuid::Uuid::new_v4().to_string());
    assert!(success, "{unchanged}");
    assert_eq!(unchanged["code"], "unchanged");
    let (_, diagnose) = host(&["diagnose"]);
    assert_eq!(diagnose["daemon"]["sample"]["indexed_commit"], after);
    assert_eq!(diagnose["daemon"]["sample"]["source_mode"], "snapshot");
    let lock = fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join("operations/mutation.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let (success, busy) = action("retry-index", &uuid::Uuid::new_v4().to_string());
    assert!(!success);
    assert_eq!(busy["code"], "operation_busy");
    assert_eq!(action("refresh", &initial).1, first);
    drop(lock);

    fs::write(dir.path().join("hold-operation"), "").unwrap();
    let interrupted_id = uuid::Uuid::new_v4().to_string();
    let interrupted = AsyncCommand::new(env!("CARGO_BIN_EXE_know-things"))
        .env(
            "KNOW_THINGS_FIXTURE_BINARY",
            env!("CARGO_BIN_EXE_know-things"),
        )
        .args(["ops", "retry-index", "--config"])
        .arg(&config)
        .args(["--operation-id", &interrupted_id])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = i32::try_from(interrupted.id().unwrap()).unwrap();
    timeout(Duration::from_secs(5), async {
        while !dir.path().join("waiting-operation").exists() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let (active, observation) = action("operation", &interrupted_id);
    assert!(active);
    assert_eq!(observation["state"], "running");
    // This PID belongs to the fixture child spawned above.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let exit = timeout(Duration::from_secs(5), interrupted.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(!exit.status.success());
    let terminal: Value = serde_json::from_slice(&exit.stdout).unwrap();
    assert_eq!(terminal["state"], "interrupted");
    assert_eq!(terminal["indexed_commit"], after);
    assert_eq!(action("operation", &interrupted_id).1, terminal);
    assert_eq!(
        cli(&["search", "--socket", socket, "UpdatedThing"])["status"],
        "ok"
    );

    let lost_id = uuid::Uuid::new_v4().to_string();
    let mut orphaned = observation;
    orphaned["operation_id"] = json!(lost_id);
    fs::write(
        dir.path()
            .join("operations")
            .join(format!("{lost_id}.json")),
        orphaned.to_string(),
    )
    .unwrap();
    let (found, recovered) = action("operation", &lost_id);
    assert!(found);
    assert_eq!(recovered["state"], "interrupted");
    assert_eq!(recovered["indexed_commit"], after);
    daemon.kill().await.unwrap();
}
