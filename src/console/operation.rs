use super::{MAX_JSON_BYTES, Profile, Source, docker};
use crate::{
    local_file, source,
    timeline::{self, OperationPhase as Phase, Outcome as State},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{ErrorKind, Seek},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::Command,
    time::{Instant, sleep, timeout},
};
use uuid::Uuid;

const HISTORY: usize = 32;
const SOURCE_DEADLINE: Duration = Duration::from_secs(120);
const INDEX_DEADLINE: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Refresh,
    RetryIndex,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Operation {
    schema_version: u32,
    operation_id: String,
    action: Action,
    phase: Phase,
    state: State,
    started_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    target_commit: Option<String>,
    indexed_commit: Option<String>,
    code: Option<String>,
}

impl Operation {
    fn start(id: Uuid, action: Action) -> Self {
        let now = timeline::now_ms();
        Self {
            schema_version: 1,
            operation_id: id.to_string(),
            action,
            phase: match action {
                Action::Refresh => Phase::Starting,
                Action::RetryIndex => Phase::WaitingForIndex,
            },
            state: State::Running,
            started_at_unix_ms: now,
            updated_at_unix_ms: now,
            target_commit: None,
            indexed_commit: None,
            code: None,
        }
    }

    pub fn succeeded(&self) -> bool {
        self.state == State::Succeeded
    }

    fn remember(&mut self, path: &Path) -> Result<()> {
        self.updated_at_unix_ms = timeline::now_ms();
        local_file::write_json(path, self, MAX_JSON_BYTES as u64, 0o600)
    }

    fn fail(&mut self, code: &str) {
        self.state = State::Failed;
        self.code = Some(code.to_owned());
    }
}

fn state_path(profile: &Profile, id: Uuid) -> PathBuf {
    profile.state_dir.join(format!("{id}.json"))
}

fn ensure_state_dir(path: &Path) -> Result<()> {
    local_file::private_directory(path, true).context("console state directory must be private")
}

fn mutation_lock(directory: &Path) -> Result<Option<File>> {
    let lock = local_file::open_lock(&directory.join("mutation.lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn load_operation(path: &Path) -> Result<Option<Operation>> {
    match super::read_json(path) {
        Ok(value) => Ok(Some(value)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

async fn reconcile_operation(
    profile: &Profile,
    path: &Path,
    mut op: Operation,
) -> Result<Operation> {
    if op.state == State::Running {
        op.state = State::Interrupted;
        op.code = Some("host_interrupted".to_owned());
        op.indexed_commit = docker::indexed_commit(&profile.docker)
            .await
            .or(op.indexed_commit);
        op.remember(path)?;
    }
    Ok(op)
}

pub(super) async fn load(profile: &Profile, id: Uuid) -> Result<Operation> {
    let path = state_path(profile, id);
    let op = load_operation(&path)?.context("console operation not found")?;
    if op.state != State::Running {
        return Ok(op);
    }
    let Some(_lock) = mutation_lock(&profile.state_dir)? else {
        return Ok(op);
    };
    let current = load_operation(&path)?.context("console operation no longer retained")?;
    reconcile_operation(profile, &path, current).await
}

pub async fn run_action(profile: &Profile, id: Uuid, action: Action) -> Result<Operation> {
    ensure_state_dir(&profile.state_dir)?;
    let path = state_path(profile, id);
    let Some(_lock) = mutation_lock(&profile.state_dir)? else {
        if let Some(previous) = load_operation(&path)? {
            return Ok(previous);
        }
        let mut result = Operation::start(id, action);
        result.fail("operation_busy");
        return Ok(result);
    };
    if let Some(previous) = load_operation(&path)? {
        return reconcile_operation(profile, &path, previous).await;
    }

    let mut op = Operation::start(id, action);
    op.remember(&path)?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = tokio::select! {
        result = run(&mut op, &path, profile) => result,
        _ = interrupt.recv() => Ok(Some("host_interrupted")),
        _ = terminate.recv() => Ok(Some("host_interrupted")),
    };
    if let Some(code) = result.unwrap_or(Some("action_failed")) {
        op.fail(code);
        if code == "host_interrupted" {
            op.state = State::Interrupted;
        }
        op.indexed_commit = docker::indexed_commit(&profile.docker)
            .await
            .or(op.indexed_commit);
    }
    op.remember(&path)?;
    prune(&profile.state_dir, &path);
    Ok(op)
}

fn prune(directory: &Path, current: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut rows: Vec<_> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()?.to_str()? != "json"
                || Uuid::parse_str(path.file_stem()?.to_str()?).is_err()
            {
                return None;
            }
            let record = load_operation(&path).ok().flatten()?;
            (record.state != State::Running).then_some((
                path.clone(),
                path == current,
                record.updated_at_unix_ms,
            ))
        })
        .collect();
    rows.sort_unstable_by_key(|(_, is_current, time)| std::cmp::Reverse((*is_current, *time)));
    for (path, _, _) in rows.into_iter().skip(HISTORY) {
        let _ = fs::remove_file(path);
    }
}

async fn run(op: &mut Operation, path: &Path, profile: &Profile) -> Result<Option<&'static str>> {
    if let Action::Refresh = op.action {
        match update_source(op, path, &profile.source).await? {
            Ok(commit) => op.target_commit = Some(commit),
            Err(code) => return Ok(Some(code)),
        }
    }
    op.phase = Phase::WaitingForIndex;
    op.remember(path)?;
    let mut request = vec!["admin", "refresh", "--operation-id", &op.operation_id];
    if let Some(commit) = &op.target_commit {
        request.extend(["--expected-commit", commit]);
    }
    let Ok(mut progress) = docker::index_progress(&profile.docker, &request).await else {
        return Ok(Some("admin_unreachable"));
    };
    let completion = timeout(INDEX_DEADLINE, async {
        loop {
            op.phase = progress.phase;
            if progress.indexed_commit.is_some() || progress.state != State::Running {
                op.indexed_commit.clone_from(&progress.indexed_commit);
            }
            if op.target_commit.is_none() {
                op.target_commit.clone_from(&progress.target_commit);
            }
            if progress.state != State::Running {
                op.state = progress.state;
                op.code = progress.code;
                return Ok::<_, anyhow::Error>(());
            }
            op.remember(path)?;
            sleep(POLL_INTERVAL).await;
            if let Ok(next) = docker::index_progress(
                &profile.docker,
                &["admin", "operation", "--operation-id", &op.operation_id],
            )
            .await
            {
                progress = next;
            }
        }
    })
    .await;
    match completion {
        Ok(result) => result?,
        Err(_) => return Ok(Some("index_timeout")),
    }
    if op.succeeded() && (op.target_commit.is_none() || op.target_commit != op.indexed_commit) {
        return Ok(Some("index_revision_mismatch"));
    }
    Ok(None)
}

async fn update_source(
    op: &mut Operation,
    path: &Path,
    profile: &Source,
) -> Result<Result<String, &'static str>> {
    let (source_dir, managed, mut child) = source_command(profile, &op.operation_id)?;
    let end = Instant::now() + SOURCE_DEADLINE;
    let exit = loop {
        if let Ok(stage) = source::load_stages(source_dir)
            && stage.operation_id.as_deref() == Some(op.operation_id.as_str())
        {
            let phase = if managed
                && stage.fetch.last_attempt.as_ref().is_some_and(|attempt| {
                    attempt.outcome != timeline::Outcome::Succeeded
                        || stage.export.last_attempt.as_ref().is_none_or(|export| {
                            export.started_at_unix_ms
                                < attempt.finished_at_unix_ms.unwrap_or(u64::MAX)
                        })
                }) {
                Phase::Fetching
            } else {
                Phase::Exporting
            };
            if op.phase != phase {
                op.phase = phase;
                op.remember(path)?;
            }
        }
        if let Some(exit) = child.child.try_wait()? {
            child.group = None;
            break exit;
        }
        if Instant::now() >= end {
            child.stop().await?;
            source::interrupt_stages(source_dir, &op.operation_id, "source_timeout");
            return Ok(Err("source_timeout"));
        }
        sleep(POLL_INTERVAL).await;
    };
    if !exit.success() {
        child.stderr.rewind()?;
        let stderr = local_file::read_limited(&child.stderr, MAX_JSON_BYTES)?;
        if String::from_utf8_lossy(&stderr).contains(source::SOURCE_BUSY) {
            return Ok(Err("operation_busy"));
        }
        let code = match op.phase {
            Phase::Fetching => "fetch_failed",
            Phase::Exporting => "export_failed",
            _ => "source_failed",
        };
        return Ok(Err(code));
    }
    child.stdout.rewind()?;
    let stdout = local_file::read_limited(&child.stdout, MAX_JSON_BYTES)?;
    let reply: serde_json::Value = serde_json::from_slice(&stdout)?;
    let key = if managed { "upstream_commit" } else { "commit" };
    Ok(reply[key]
        .as_str()
        .filter(|commit| source::is_oid(commit))
        .map(str::to_owned)
        .ok_or("source_response_invalid"))
}

struct SourceChild {
    child: tokio::process::Child,
    group: Option<i32>,
    stdout: File,
    stderr: File,
}

impl SourceChild {
    async fn stop(&mut self) -> Result<()> {
        self.signal();
        let _ = self.child.wait().await?;
        Ok(())
    }

    fn signal(&mut self) {
        if let Some(group) = self.group.take() {
            // The child is placed in its own group before exec. This signal covers only
            // the source command and the Git/SSH children it launched for this job.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
}

impl Drop for SourceChild {
    fn drop(&mut self) {
        self.signal();
    }
}

fn source_command<'a>(profile: &'a Source, id: &str) -> Result<(&'a Path, bool, SourceChild)> {
    let mut command = Command::new(std::env::current_exe()?);
    let (dir, managed) = match profile {
        Source::Snapshot {
            checkout,
            git_ref,
            repo_url,
            source_dir,
        } => {
            command.args(["export", "--repo"]).arg(checkout).args([
                "--git-ref",
                git_ref,
                "--repo-url",
                repo_url,
            ]);
            (source_dir, false)
        }
        Source::Managed {
            mirror,
            remote,
            repo_url,
            source_dir,
        } => {
            command.args(["sync", "--mirror"]).arg(mirror).args([
                "--remote",
                remote,
                "--repo-url",
                repo_url,
            ]);
            (source_dir, true)
        }
    };
    let stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    command.as_std_mut().process_group(0);
    let child = command
        .arg("--source-dir")
        .arg(dir)
        .env(source::OPERATION_ENV, id)
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .kill_on_drop(true)
        .spawn()?;
    let group = child.id().and_then(|id| i32::try_from(id).ok());
    Ok((
        dir,
        managed,
        SourceChild {
            child,
            group,
            stdout,
            stderr,
        },
    ))
}
