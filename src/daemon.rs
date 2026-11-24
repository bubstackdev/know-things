use crate::{
    admin,
    index::{self, QueryControl},
    local_file,
    refresh::{RefreshControl, RefreshRequest},
    source::{self, Snapshot, SourceMode, SyncStatus},
    telemetry::{self, Details, Health, HealthFreshness, HealthReason, HealthState, Publisher},
    timeline::{self, Attempt, Outcome, SourceStages, Stage},
    wire::{self, MAX_REQUEST, MAX_RESPONSE, Request, Response},
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json, to_value};
use std::{
    fs::{self, File},
    io::ErrorKind,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixStream as StdUnixStream,
    },
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    net::{UnixListener, UnixStream},
    signal::unix::{SignalKind, signal},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{self, MissedTickBehavior, timeout},
};

const MAX_CONCURRENT: usize = 8;
pub const BLOCKING_WORKERS: usize = MAX_CONCURRENT + 2;
const QUERY_DEADLINE: Duration = Duration::from_secs(2);
const LEGACY_STALE_AFTER: u64 = 15 * 60;
const MISSED_CHECKS_BEFORE_STALE: u64 = 3;
const REPORT_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone, Default)]
struct SourceState {
    exported_commit: Option<String>,
    last_index_error: Option<&'static str>,
    sync: Option<SyncStatus>,
    sync_metadata_error: Option<&'static str>,
    index_initialized: bool,
    source_stages: SourceStages,
    index_stage: Stage,
    cached_index: Option<index::IndexStatus>,
    index_readable: bool,
    index_generation: u64,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum FreshnessStatus {
    Current,
    Stale,
    UpstreamUnknown,
    Unavailable,
}

#[derive(Serialize)]
struct Freshness<'a> {
    status: FreshnessStatus,
    source_mode: SourceMode,
    exported_commit: Option<&'a str>,
    upstream_commit: Option<&'a str>,
    last_upstream_check_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    check_interval_seconds: Option<u32>,
    last_index_error: Option<&'static str>,
    last_sync_error: Option<&'a str>,
}

impl SourceState {
    fn managed_source(&self) -> bool {
        self.sync
            .as_ref()
            .map_or(self.source_stages.managed, |sync| sync.managed)
    }

    fn indexed_commit(&self) -> Option<String> {
        self.cached_index
            .as_ref()
            .and_then(|index| index.indexed_commit.clone())
    }

    fn cache_index(&mut self, index: Option<index::IndexStatus>) {
        self.index_generation = self.index_generation.wrapping_add(1);
        self.index_readable = index.is_some();
        if index.is_some() {
            self.cached_index = index;
        }
    }

    fn admin_status(&self, instance_id: &str) -> Value {
        let observed = self
            .cached_index
            .as_ref()
            .filter(|_| self.index_readable)
            .cloned();
        let (health, _) = self.ops_status(observed);
        let status = self.freshness(
            self.cached_index
                .as_ref()
                .and_then(|info| info.indexed_commit.as_deref()),
        );
        let exported = self
            .exported_commit
            .as_deref()
            .or_else(|| {
                self.source_stages
                    .export
                    .last_success
                    .as_ref()
                    .and_then(|entry| entry.commit.as_deref())
            })
            .filter(|_| {
                !matches!(
                    self.last_index_error,
                    Some("invalid_snapshot" | "snapshot_missing")
                )
            });
        json!({
            "instance_id": instance_id,
            "sampled_at_unix_ms": timeline::now_ms(),
            "source_mode": if self.source_stages.managed { SourceMode::Managed } else { status.source_mode },
            "health": health,
            "index_readable": self.index_readable,
            "upstream_commit": self.source_stages.fetch.last_success.as_ref()
                .and_then(|attempt| attempt.commit.as_deref()).or(status.upstream_commit),
            "exported_commit": exported,
            "indexed_commit": self.cached_index.as_ref().and_then(|info| info.indexed_commit.as_deref()),
            "files": self.cached_index.as_ref().map(|info| info.files),
            "passages": self.cached_index.as_ref().map(|info| info.passages),
            "stages": {
                "fetch": self.source_stages.fetch,
                "export": self.source_stages.export,
                "index": self.index_stage,
            },
        })
    }
    fn freshness(&self, current: Option<&str>) -> Freshness<'_> {
        let sync = self.sync.as_ref();
        let managed = self.managed_source();
        let upstream_commit = sync.and_then(|s| s.upstream_commit.as_deref());
        let checked_at = sync.and_then(|s| s.checked_at_unix);
        let check_interval_seconds = if managed {
            self.source_stages
                .check_interval_seconds
                .or_else(|| sync.and_then(|s| s.check_interval_seconds))
                .map(|interval| interval.get())
        } else {
            None
        };
        let stale_after = check_interval_seconds.map_or(LEGACY_STALE_AFTER, |seconds| {
            u64::from(seconds) * MISSED_CHECKS_BEFORE_STALE
        });
        let last_sync_error = self
            .sync_metadata_error
            .or_else(|| sync.and_then(|s| s.last_error.as_deref()));
        let exported_commit = self.exported_commit.as_deref();
        let status = if current.is_none() {
            FreshnessStatus::Unavailable
        } else if self.last_index_error.is_some()
            || last_sync_error.is_some()
            || (exported_commit.is_some() && current != exported_commit)
            || (managed
                && (upstream_commit != current
                    || checked_at
                        .is_none_or(|ts| source::now_unix().saturating_sub(ts) > stale_after)))
        {
            FreshnessStatus::Stale
        } else if managed {
            FreshnessStatus::Current
        } else {
            FreshnessStatus::UpstreamUnknown
        };
        Freshness {
            status,
            source_mode: if managed {
                SourceMode::Managed
            } else {
                SourceMode::Snapshot
            },
            exported_commit,
            upstream_commit,
            last_upstream_check_unix: checked_at,
            check_interval_seconds,
            last_index_error: self.last_index_error,
            last_sync_error,
        }
    }

    fn ops_status(&self, index: Option<index::IndexStatus>) -> (Health, Details) {
        let commit = index
            .as_ref()
            .and_then(|data| data.indexed_commit.as_deref());
        let status = self.freshness(commit);
        let index_failure = (self.index_initialized && index.is_none())
            || status
                .last_index_error
                .is_some_and(|error| commit.is_some() || error != "snapshot_missing");
        let (state, freshness, reason) = match status.status {
            FreshnessStatus::Current => (HealthState::Ready, HealthFreshness::Current, None),
            FreshnessStatus::UpstreamUnknown => {
                (HealthState::Ready, HealthFreshness::Unknown, None)
            }
            FreshnessStatus::Stale => {
                let reason = if index_failure {
                    HealthReason::IndexError
                } else if status.last_sync_error.is_some() {
                    HealthReason::SyncError
                } else if status
                    .exported_commit
                    .is_some_and(|sha| Some(sha) != commit)
                    || status
                        .upstream_commit
                        .is_some_and(|sha| Some(sha) != commit)
                {
                    HealthReason::IndexLag
                } else {
                    HealthReason::UpstreamCheckOverdue
                };
                (HealthState::Degraded, HealthFreshness::Stale, Some(reason))
            }
            FreshnessStatus::Unavailable if index_failure => (
                HealthState::Unavailable,
                HealthFreshness::Unknown,
                Some(HealthReason::IndexError),
            ),
            FreshnessStatus::Unavailable if status.last_sync_error.is_some() => (
                HealthState::Unavailable,
                HealthFreshness::Unknown,
                Some(HealthReason::SyncError),
            ),
            FreshnessStatus::Unavailable => (
                HealthState::Starting,
                HealthFreshness::Unknown,
                Some(HealthReason::WaitingForIndex),
            ),
        };
        (
            Health {
                state,
                freshness,
                reason,
            },
            Details {
                files: index.as_ref().map(|data| data.files),
                passages: index.map(|data| data.passages),
                source_mode: status.source_mode,
            },
        )
    }
}

#[derive(PartialEq, Eq)]
struct Stamp {
    inode: u64,
    bytes: u64,
    modified: Option<std::time::SystemTime>,
}

impl Stamp {
    fn read(path: &Path) -> std::io::Result<Self> {
        let meta = path.metadata()?;
        Ok(Self {
            inode: meta.ino(),
            bytes: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

struct Refresh {
    db: Option<rusqlite::Connection>,
    database: PathBuf,
    source_dir: PathBuf,
    state: Arc<RwLock<SourceState>>,
    last_snapshot: Option<Stamp>,
    retry_after: Option<Instant>,
    retry_delay: Duration,
}

impl Refresh {
    fn read_source_metadata(&self) {
        let sync = source::load_sync(&self.source_dir);
        let stages = source::load_stages(&self.source_dir);
        let missing = |error: &anyhow::Error| {
            error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == ErrorKind::NotFound)
        };
        let mut state = self.state.write().unwrap();
        let was_managed = state.managed_source();
        let stage_error = match stages {
            Ok(stages) => {
                state.source_stages = stages;
                None
            }
            Err(error) if missing(&error) => {
                state.source_stages = SourceStages::default();
                None
            }
            Err(error) => Some((
                "source_observations_unreadable",
                "source-observations.json",
                error,
            )),
        };
        let sync_error = match sync {
            Ok(sync) => {
                state.sync = Some(sync);
                None
            }
            Err(error) if missing(&error) && !was_managed && !state.managed_source() => None,
            Err(error) => Some(("sync_status_unreadable", source::SYNC_FILE, error)),
        };
        let failure = sync_error.or(stage_error);
        if let Some((code, file, error)) = &failure
            && state.sync_metadata_error != Some(*code)
        {
            eprintln!("KB could not read {file}: {error:#}");
        }
        state.sync_metadata_error = failure.map(|(code, _, _)| code);
    }

    fn tick(
        &mut self,
        explicit: Option<(&RefreshControl, &RefreshRequest)>,
    ) -> Result<Option<&'static str>, &'static str> {
        self.read_source_metadata();
        let started = Instant::now();
        if self.db.is_none() {
            match index::initialize(&self.database) {
                Ok(mut db) => {
                    let cached = index::status(&mut db).ok();
                    self.db = Some(db);
                    let mut state = self.state.write().unwrap();
                    state.index_initialized = true;
                    state.last_index_error = None;
                    state.cache_index(cached);
                }
                Err(error) => {
                    if self.state.read().unwrap().last_index_error != Some("index_open_failed") {
                        eprintln!("KB index initialization failed: {error:#}");
                    }
                    if explicit.is_some()
                        || self.state.read().unwrap().last_index_error != Some("index_open_failed")
                    {
                        self.index_failure(started, Attempt::running(None), "index_open_failed");
                    }
                    return Err("index_open_failed");
                }
            }
        }
        let stamp = match Stamp::read(&self.source_dir.join(source::SNAPSHOT_FILE)) {
            Ok(stamp) => stamp,
            Err(error) => {
                self.last_snapshot = None;
                let code = if error.kind() == ErrorKind::NotFound {
                    "snapshot_missing"
                } else {
                    "snapshot_unreadable"
                };
                if explicit.is_some() || self.state.read().unwrap().last_index_error != Some(code) {
                    self.index_failure(started, Attempt::running(None), code);
                }
                return Err(code);
            }
        };
        if explicit.is_none() && self.retry_after.is_some_and(|retry| Instant::now() < retry) {
            return Ok(None);
        }
        if explicit.is_none() && self.last_snapshot.as_ref() == Some(&stamp) {
            return Ok(None);
        }
        let mut attempt = Attempt::running(None);
        self.state
            .write()
            .unwrap()
            .index_stage
            .record(attempt.clone());
        let snapshot = match Snapshot::load(&self.source_dir) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                eprintln!("KB refresh rejected the source snapshot: {error:#}");
                self.state.write().unwrap().exported_commit = None;
                self.index_failure(started, attempt, "invalid_snapshot");
                self.last_snapshot = Some(stamp);
                return Err("invalid_snapshot");
            }
        };
        let revision = snapshot.revision();
        attempt.commit = Some(revision.clone());
        {
            let mut state = self.state.write().unwrap();
            state.exported_commit = Some(revision.clone());
            if explicit.is_some_and(|(_, request)| {
                request
                    .expected_commit
                    .as_deref()
                    .is_some_and(|expected| expected != revision)
            }) {
                attempt.finish(started, Outcome::Failed, Some("snapshot_superseded"));
                state.index_stage.record(attempt);
                return Err("snapshot_superseded");
            }
            state.index_stage.record(attempt.clone());
        }
        if let Some((control, request)) = explicit {
            control.indexing(request.id, &revision);
        }
        let db = self.db.as_mut().expect("refresh initialized the index");
        let same_revision = index::revision(db).is_ok_and(|indexed| {
            indexed.is_some_and(|(commit, url)| commit == revision && url == snapshot.repo_url)
        });
        match index::ingest(db, &snapshot) {
            Ok(result) => {
                self.retry_after = None;
                self.retry_delay = Duration::from_secs(30);
                let cached = index::status(db).ok();
                let readable = explicit.is_none()
                    || index::reader(&self.database, None)
                        .and_then(|mut reader| index::status(&mut reader))
                        .is_ok_and(|seen| seen.indexed_commit.as_deref() == Some(&revision));
                let unchanged =
                    (same_revision && result.updated_files == 0 && result.removed_files == 0)
                        .then_some("unchanged");
                let mut state = self.state.write().unwrap();
                state.last_index_error = None;
                state.cache_index(cached);
                if !readable {
                    state.index_readable = false;
                }
                attempt.updated_files = Some(result.updated_files);
                attempt.removed_files = Some(result.removed_files);
                attempt.finish(started, Outcome::Succeeded, unchanged);
                state.index_stage.record(attempt);
                self.last_snapshot = Some(stamp);
                eprintln!(
                    "KB indexed {} ({} files, {} updated, {} removed)",
                    result.commit, result.files, result.updated_files, result.removed_files
                );
                if readable {
                    Ok(unchanged)
                } else {
                    Err("index_unavailable")
                }
            }
            Err(error) => {
                self.retry_after = Some(Instant::now() + self.retry_delay);
                self.retry_delay = (self.retry_delay * 2).min(Duration::from_secs(300));
                self.index_failure(started, attempt, "index_update_failed");
                eprintln!("KB refresh could not update index: {error:#}");
                Err("index_update_failed")
            }
        }
    }

    fn index_failure(&self, started: Instant, mut attempt: Attempt, code: &'static str) {
        attempt.finish(started, Outcome::Failed, Some(code));
        let mut state = self.state.write().unwrap();
        state.last_index_error = Some(code);
        state.index_stage.record(attempt);
    }
}

struct SocketGuard {
    path: PathBuf,
    _lock: File,
}

impl SocketGuard {
    fn bind(path: PathBuf) -> Result<(UnixListener, Self)> {
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .context("socket path requires a private parent directory")?;
        local_file::private_directory(dir, false)
            .context("socket parent directory must be a real private directory (chmod 700)")?;
        let lock = local_file::open_lock(&path.with_extension("lock"))?;
        lock.try_lock()
            .context("another KB daemon owns this socket")?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_socket(),
                    "socket path is occupied by a non-socket file"
                );
                match StdUnixStream::connect(&path) {
                    Ok(_) => anyhow::bail!("socket path belongs to a running service"),
                    Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
                        fs::remove_file(&path)?
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&path)?;
        let guard = Self { path, _lock: lock };
        fs::set_permissions(&guard.path, fs::Permissions::from_mode(0o600))?;
        Ok((listener, guard))
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub async fn serve(
    database: PathBuf,
    source_dir: PathBuf,
    socket: PathBuf,
    refresh_interval: Duration,
) -> Result<()> {
    // Reserve workers for queries, refresh, and reporting before admitting MCP sessions.
    let mut release = Vec::with_capacity(BLOCKING_WORKERS);
    let mut ready = Vec::with_capacity(BLOCKING_WORKERS);
    for _ in 0..BLOCKING_WORKERS {
        let (started, signal) = tokio::sync::oneshot::channel();
        let (unblock, blocked) = tokio::sync::oneshot::channel::<()>();
        tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            let _ = blocked.blocking_recv();
        });
        release.push(unblock);
        ready.push(signal);
    }
    timeout(Duration::from_secs(2), async {
        for worker in ready {
            worker.await?;
        }
        Ok::<_, tokio::sync::oneshot::error::RecvError>(())
    })
    .await
    .context("could not reserve the KB daemon's blocking workers")??;
    drop(release);
    let report_path = socket.with_file_name(telemetry::SNAPSHOT_FILE);
    let admin_path = socket.with_file_name(admin::SOCKET_NAME);
    ensure!(admin_path != socket, "query and admin sockets must differ");
    let (listener, _socket) = SocketGuard::bind(socket)?;
    let (admin_listener, _admin_socket) = SocketGuard::bind(admin_path)?;
    let state = Arc::new(RwLock::new(SourceState::default()));
    let publisher = Arc::new(Publisher::new());
    let (control, mut pending) = RefreshControl::new();
    let admin_control = control.clone();
    let admin_state = state.clone();
    let admin_publisher = publisher.clone();
    let mut admin_task = tokio::spawn(admin::serve(admin_listener, move |request| match request {
        admin::Request::Status => {
            let mut status = admin_state
                .read()
                .unwrap()
                .admin_status(admin_publisher.instance_id());
            status["metrics"] = admin_publisher.diagnostic_metrics();
            status
        }
        admin::Request::Refresh {
            operation_id,
            expected_commit,
        } => {
            let Ok(id) = uuid::Uuid::parse_str(&operation_id) else {
                return json!({"code":"invalid_operation_id"});
            };
            let indexed = admin_state.read().unwrap().indexed_commit();
            json!(admin_control.request(id, expected_commit, indexed))
        }
        admin::Request::Operation { operation_id } => {
            let Ok(id) = uuid::Uuid::parse_str(&operation_id) else {
                return json!({"code":"invalid_operation_id"});
            };
            let indexed = admin_state.read().unwrap().indexed_commit();
            json!(admin_control.status(id, indexed))
        }
    }));
    let (stop_reporting, reporting_stopped) = tokio::sync::oneshot::channel();
    let reporting = tokio::spawn(report(
        database.clone(),
        state.clone(),
        publisher.clone(),
        report_path,
        reporting_stopped,
    ));
    let refresh = Refresh {
        db: None,
        database: database.clone(),
        source_dir,
        state: state.clone(),
        last_snapshot: None,
        retry_after: None,
        retry_delay: Duration::from_secs(30),
    };
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let mut refresh_task = tokio::spawn(async move {
        let mut refresh = Some(refresh);
        let mut interval = time::interval(refresh_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let job = tokio::select! {
                _ = &mut stopped => break,
                Some(job) = pending.recv() => Some(job),
                _ = interval.tick() => None,
            };
            let mut work = refresh.take().expect("refresh owns its database");
            let control = control.clone();
            refresh = Some(
                tokio::task::spawn_blocking(move || {
                    let result = work.tick(job.as_ref().map(|job| (&control, job)));
                    if let Some(job) = job {
                        let indexed = work.state.read().unwrap().indexed_commit();
                        control.finish(job.id, result, indexed);
                    }
                    work
                })
                .await
                .context("KB refresh task failed")?,
            );
        }
        Ok::<_, anyhow::Error>(())
    });
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let access = Arc::new(Semaphore::new(MAX_CONCURRENT));
    let mut active = tokio::task::JoinSet::new();
    let mut refresh_outcome = None;
    let mut admin_finished = false;
    eprintln!("KB daemon listening on its local socket");
    let outcome = loop {
        tokio::select! {
            _ = interrupt.recv() => break Ok(()),
            _ = terminate.recv() => break Ok(()),
            result = &mut refresh_task => {
                refresh_outcome = Some(result);
                break Ok(());
            }
            result = &mut admin_task => {
                admin_finished = true;
                break result.context("KB admin listener failed").and_then(|result| result);
            }
            _ = active.join_next(), if !active.is_empty() => {}
            incoming = listener.accept() => {
                let (mut stream, _) = match incoming {
                    Ok(socket) => socket,
                    Err(error) if wire::retryable_accept(&error) => {
                        eprintln!("KB query socket temporarily unable to accept: {error}");
                        time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                    Err(error) => break Err(error.into()),
                };
                if let Some(permit) = query_slot(&mut stream, &access).await {
                    let path = database.clone();
                    let state = state.clone();
                    let publisher = publisher.clone();
                    active.spawn(async move {
                        handle(stream, path, state, publisher, permit).await;
                    });
                }
            }
        }
    };
    drop(listener);
    if !admin_finished {
        admin_task.abort();
        let _ = admin_task.await;
    }
    let _ = stop_reporting.send(());
    let _ = reporting.await;
    let _ = stop.send(());
    active.shutdown().await;
    match refresh_outcome {
        Some(result) => result??,
        None => refresh_task.await??,
    }
    outcome?;
    Ok(())
}

async fn report(
    database: PathBuf,
    state: Arc<RwLock<SourceState>>,
    publisher: Arc<Publisher>,
    path: PathBuf,
    mut stopped: tokio::sync::oneshot::Receiver<()>,
) {
    let mut interval = time::interval(REPORT_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut reported_failure = false;
    loop {
        tokio::select! {
            _ = &mut stopped => break,
            _ = interval.tick() => {}
        }
        let (database, state, publisher, path) = (
            database.clone(),
            state.clone(),
            publisher.clone(),
            path.clone(),
        );
        let result = tokio::task::spawn_blocking(move || {
            let generation = state.read().unwrap().index_generation;
            let index = index::reader(&database, None)
                .and_then(|mut db| index::status(&mut db))
                .ok();
            let (health, details) = {
                let mut state = state.write().unwrap();
                if state.index_generation == generation {
                    state.cache_index(index.clone());
                }
                state.ops_status(index)
            };
            publisher.publish(&path, health, details)
        })
        .await;
        let failed = !matches!(result, Ok(Ok(())));
        if failed && !reported_failure {
            eprintln!("KB could not publish its local Know Ops report");
        }
        reported_failure = failed;
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

async fn query_slot(
    stream: &mut UnixStream,
    slots: &Arc<Semaphore>,
) -> Option<OwnedSemaphorePermit> {
    if let Ok(permit) = slots.clone().try_acquire_owned() {
        return Some(permit);
    }
    let busy = Response::error("busy", "KB is at its concurrency limit. Retry shortly.");
    let _ = timeout(
        Duration::from_millis(5),
        wire::send(stream, &busy, MAX_RESPONSE),
    )
    .await;
    None
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

async fn handle(
    mut stream: UnixStream,
    database: PathBuf,
    state: Arc<RwLock<SourceState>>,
    publisher: Arc<Publisher>,
    permit: OwnedSemaphorePermit,
) {
    let request: Request = match timeout(
        Duration::from_secs(1),
        wire::receive(&mut stream, MAX_REQUEST),
    )
    .await
    {
        Ok(Ok(request)) => request,
        Ok(Err(_)) => {
            let reply = Response::error("invalid_request", "Invalid KB request.");
            let _ = timeout(
                Duration::from_millis(100),
                wire::send(&mut stream, &reply, MAX_RESPONSE),
            )
            .await;
            return;
        }
        Err(_) => return,
    };
    let sample = publisher.track(&request);
    let control = QueryControl {
        cancelled: Arc::new(AtomicBool::new(false)),
        deadline: Instant::now() + QUERY_DEADLINE,
    };
    let _cancel = CancelOnDrop(control.cancelled.clone());
    let mut job = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        execute(&database, request, &state, control)
    });
    let mut extra = [0];
    let response = tokio::select! {
        _ = stream.read(&mut extra) => return,
        result = timeout(QUERY_DEADLINE, &mut job) => match result {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => Response::error("service_error", "KB query worker failed."),
            Err(_) => Response::error("deadline_exceeded", "KB query exceeded its deadline."),
        }
    };
    if let Some(sample) = sample {
        sample.finish(&response);
    }
    let _ = timeout(
        Duration::from_millis(100),
        wire::send(&mut stream, &response, MAX_RESPONSE),
    )
    .await;
}

fn execute(
    database: &Path,
    request: Request,
    state: &RwLock<SourceState>,
    control: QueryControl,
) -> Response {
    let valid = match &request {
        Request::Search(args) => index::validate_search(&args.query, args.limit),
        Request::Read(args) => {
            index::validate_read(&args.path, &args.commit, args.start_line, args.end_line)
        }
        Request::Status => Ok(()),
    };
    if let Err(error) = valid {
        return Response::error("invalid_arguments", error.to_string());
    }
    let result = (|| {
        let mut db = index::reader(database, Some(control.clone()))?;
        let mut value = match request {
            Request::Search(args) => to_value(index::search(&mut db, &args.query, args.limit)?)?,
            Request::Read(args) => to_value(index::read(
                &mut db,
                &args.path,
                &args.commit,
                args.start_line,
                args.end_line,
            )?)?,
            Request::Status => to_value(index::status(&mut db)?)?,
        };
        let revision = value.get("indexed_commit").and_then(Value::as_str);
        value["freshness"] = to_value(state.read().unwrap().freshness(revision))?;
        Ok::<_, anyhow::Error>(value)
    })();
    match result {
        Ok(value) => Response::Ok(value),
        Err(error) => query_error(&error, &control),
    }
}

fn query_error(error: &anyhow::Error, control: &QueryControl) -> Response {
    use rusqlite::{Error as SqlError, ErrorCode};
    let code = match error.downcast_ref::<SqlError>() {
        Some(SqlError::SqliteFailure(failure, _)) => Some(failure.code),
        _ => None,
    };
    if Instant::now() >= control.deadline {
        return Response::error("deadline_exceeded", "KB query exceeded its deadline.");
    }
    if control.cancelled.load(Ordering::Relaxed) {
        return Response::error("cancelled", "KB query was cancelled.");
    }
    let (status, message) = match code {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            ("busy", "KB index is temporarily busy.")
        }
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => (
            "index_corrupt",
            "KB index is corrupt. Rebuild it from a source snapshot.",
        ),
        Some(ErrorCode::SystemIoFailure | ErrorCode::DiskFull) => (
            "index_io_error",
            "KB index storage failed. Check disk space and permissions.",
        ),
        _ => (
            "index_unavailable",
            "KB index could not answer. Check daemon status and logs.",
        ),
    };
    // SQLite diagnostics may contain SQL or user data. Log only the classified failure.
    eprintln!("KB query failed: {status} ({code:?})");
    Response::error(status, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    #[tokio::test]
    async fn saturated_query_admission_replies_busy_and_reopens_after_release() {
        let slots = Arc::new(Semaphore::new(1));
        let held = slots.try_acquire().unwrap();
        let (mut server, mut client) = UnixStream::pair().unwrap();
        assert!(query_slot(&mut server, &slots).await.is_none());
        drop(server);
        let reply: Response = wire::receive(&mut client, MAX_RESPONSE).await.unwrap();
        assert!(matches!(reply, Response::Error(error) if error.code == "busy"));
        drop(held);
        let (mut server, _client) = UnixStream::pair().unwrap();
        assert!(query_slot(&mut server, &slots).await.is_some());
    }

    #[test]
    fn query_failures_distinguish_deadline_storage_and_busy_errors() {
        let mut control = QueryControl {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + Duration::from_secs(5),
        };
        let error = |code| {
            anyhow::Error::new(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some("private query detail".to_owned()),
            ))
        };
        for (sqlite, expected) in [
            (rusqlite::ffi::SQLITE_CORRUPT, "index_corrupt"),
            (rusqlite::ffi::SQLITE_IOERR, "index_io_error"),
            (rusqlite::ffi::SQLITE_FULL, "index_io_error"),
            (rusqlite::ffi::SQLITE_BUSY, "busy"),
        ] {
            let Response::Error(failure) = query_error(&error(sqlite), &control) else {
                panic!("query failure returned success");
            };
            assert_eq!(failure.code, expected);
            assert!(!failure.message.contains("private query detail"));
        }
        control.deadline = Instant::now();
        let reply = query_error(&error(rusqlite::ffi::SQLITE_INTERRUPT), &control);
        assert!(matches!(reply, Response::Error(error) if error.code == "deadline_exceeded"));
    }

    #[test]
    fn managed_freshness_uses_the_scheduler_interval_for_missed_checks() {
        let now = source::now_unix();
        let mut state = SourceState {
            exported_commit: Some("current".to_owned()),
            sync: Some(SyncStatus {
                managed: true,
                upstream_commit: Some("current".to_owned()),
                checked_at_unix: Some(now - 1_000),
                check_interval_seconds: NonZeroU32::new(900),
                ..Default::default()
            }),
            ..Default::default()
        };
        let current = Some("current");
        assert_eq!(state.freshness(current).status, FreshnessStatus::Current);
        assert_eq!(state.freshness(current).check_interval_seconds, Some(900));
        state.sync.as_mut().unwrap().checked_at_unix = Some(now - 2_800);
        assert_eq!(state.freshness(current).status, FreshnessStatus::Stale);
        state.sync.as_mut().unwrap().checked_at_unix = Some(now - 1_000);
        state.sync.as_mut().unwrap().check_interval_seconds = None;
        assert_eq!(state.freshness(current).status, FreshnessStatus::Stale);
    }
}
