use crate::{
    local_file::{Durability, open_lock, write_atomic},
    source::SourceMode,
    wire::{NoMatchReason, Request, Response, ResultStatus},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};
use uuid::Uuid;

pub const SNAPSHOT_FILE: &str = "ops-snapshot.json";
const CONTAINER_SNAPSHOT: &str = "/run/know-things/ops-snapshot.json";
pub const INBOX_FILE: &str = "know-things-daemon.json";
const SERVICE: &str = "know-things";
const COMPONENT: &str = "daemon";
const VERSION: u64 = 1;
const MAX_BYTES: usize = 16_384;
const MAX_INTEGER: u64 = 9_007_199_254_740_991;
const READ_DEADLINE: Duration = Duration::from_secs(5);
const LATENCY_BOUNDS_US: [u128; 12] = [
    250, 500, 1_000, 2_000, 5_000, 10_000, 25_000, 50_000, 100_000, 500_000, 1_000_000, 2_000_000,
];
type Histogram = [u64; LATENCY_BOUNDS_US.len() + 1];

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
struct Counters {
    ok: u64,
    rejected: u64,
    error: u64,
    cancelled: u64,
    duration_ms_total: u64,
}

impl Counters {
    fn record(&mut self, outcome: Outcome, elapsed: Duration) {
        let count = match outcome {
            Outcome::Ok => &mut self.ok,
            Outcome::Rejected => &mut self.rejected,
            Outcome::Error => &mut self.error,
            Outcome::Cancelled => &mut self.cancelled,
        };
        *count = (*count + 1).min(MAX_INTEGER);
        let millis = elapsed.as_millis().min(u128::from(MAX_INTEGER)) as u64;
        self.duration_ms_total = (self.duration_ms_total + millis).min(MAX_INTEGER);
    }

    fn valid(&self) -> bool {
        [
            self.ok,
            self.rejected,
            self.error,
            self.cancelled,
            self.duration_ms_total,
        ]
        .into_iter()
        .all(|n| n <= MAX_INTEGER)
    }
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
struct Usage {
    kb_search: Counters,
    kb_read: Counters,
}

#[derive(Clone, Copy, Default, Serialize)]
struct SearchOutcomes {
    hits: u64,
    empty: u64,
    abstained: u64,
}

#[derive(Clone, Copy, Default, Serialize)]
struct ReadOutcomes {
    stale_revision: u64,
    truncated: u64,
}

#[derive(Clone, Copy, Default)]
struct Measurements {
    usage: Usage,
    search_latency: Histogram,
    read_latency: Histogram,
    search: SearchOutcomes,
    read: ReadOutcomes,
}

impl Measurements {
    fn record(&mut self, operation: Operation, response: Option<&Response>, elapsed: Duration) {
        let (counters, histogram) = match operation {
            Operation::Search => (&mut self.usage.kb_search, &mut self.search_latency),
            Operation::Read => (&mut self.usage.kb_read, &mut self.read_latency),
        };
        let outcome = response.map_or(Outcome::Cancelled, Outcome::from_response);
        counters.record(outcome, elapsed);
        let micros = elapsed.as_micros();
        let bucket = LATENCY_BOUNDS_US.partition_point(|bound| *bound < micros);
        histogram[bucket] = (histogram[bucket] + 1).min(MAX_INTEGER);

        let Some(Response::Ok(data)) = response else {
            return;
        };
        let count = match (operation, ResultStatus::read(data)) {
            (Operation::Search, Some(ResultStatus::Ok)) => &mut self.search.hits,
            (Operation::Search, Some(ResultStatus::NoMatch))
                if NoMatchReason::read(data) == Some(NoMatchReason::InsufficientEvidence) =>
            {
                &mut self.search.abstained
            }
            (Operation::Search, Some(ResultStatus::NoMatch)) => &mut self.search.empty,
            (Operation::Read, Some(ResultStatus::StaleRevision)) => &mut self.read.stale_revision,
            (Operation::Read, Some(ResultStatus::Ok)) if data["truncated"] == true => {
                &mut self.read.truncated
            }
            _ => return,
        };
        *count = (*count + 1).min(MAX_INTEGER);
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Search,
    Read,
}

#[derive(Clone, Copy)]
enum Outcome {
    Ok,
    Rejected,
    Error,
    Cancelled,
}

impl Outcome {
    fn from_response(response: &Response) -> Self {
        match response {
            Response::Error(error) if error.code == "invalid_arguments" => Self::Rejected,
            Response::Error(_) => Self::Error,
            Response::Ok(data) => match ResultStatus::read(data) {
                Some(ResultStatus::Ok | ResultStatus::NoMatch) => Self::Ok,
                Some(
                    ResultStatus::StaleRevision
                    | ResultStatus::NotFound
                    | ResultStatus::RangeOutOfBounds,
                ) => Self::Rejected,
                _ => Self::Error,
            },
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Starting,
    Ready,
    Degraded,
    Unavailable,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthFreshness {
    Current,
    Stale,
    Unknown,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthReason {
    IndexError,
    SyncError,
    IndexLag,
    UpstreamCheckOverdue,
    WaitingForIndex,
}

#[derive(Serialize)]
pub struct Health {
    pub state: HealthState,
    pub freshness: HealthFreshness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<HealthReason>,
}

#[derive(Serialize)]
pub struct Details {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passages: Option<u32>,
    pub source_mode: SourceMode,
}

#[derive(Serialize)]
struct Report<'a> {
    schema_version: u64,
    service: &'static str,
    component: &'static str,
    instance_id: &'a str,
    sampled_at_unix_ms: u64,
    health: Health,
    usage: Usage,
    details: Details,
}

pub struct Publisher {
    instance: String,
    measurements: Mutex<Measurements>,
}

impl Publisher {
    pub fn new() -> Self {
        Self {
            instance: Uuid::new_v4().to_string(),
            measurements: Mutex::new(Measurements::default()),
        }
    }

    pub fn instance_id(&self) -> &str {
        &self.instance
    }

    pub fn diagnostic_metrics(&self) -> Value {
        let measures = *self.measurements.lock().unwrap();
        json!({
            "usage": measures.usage,
            "latency_us": {
                "upper_bounds": LATENCY_BOUNDS_US,
                "kb_search": measures.search_latency,
                "kb_read": measures.read_latency,
            },
            "search": measures.search,
            "read": measures.read,
        })
    }

    pub fn track(&self, request: &Request) -> Option<Sample<'_>> {
        let operation = match request {
            Request::Search(_) => Operation::Search,
            Request::Read(_) => Operation::Read,
            Request::Status => return None,
        };
        Some(Sample {
            publisher: self,
            operation: Some(operation),
            started: Instant::now(),
        })
    }

    pub fn publish(&self, path: &Path, health: Health, details: Details) -> Result<()> {
        let sampled_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let usage = self.measurements.lock().unwrap().usage;
        let report = Report {
            schema_version: VERSION,
            service: SERVICE,
            component: COMPONENT,
            instance_id: &self.instance,
            sampled_at_unix_ms: u64::try_from(sampled_at)?,
            health,
            usage,
            details,
        };
        write_report(path, &serde_json::to_vec(&report)?)
    }
}

pub struct Sample<'a> {
    publisher: &'a Publisher,
    operation: Option<Operation>,
    started: Instant,
}

impl Sample<'_> {
    pub fn finish(mut self, response: &Response) {
        self.record(Some(response));
    }

    fn record(&mut self, response: Option<&Response>) {
        if let Some(operation) = self.operation.take() {
            let elapsed = self.started.elapsed();
            self.publisher
                .measurements
                .lock()
                .unwrap()
                .record(operation, response, elapsed);
        }
    }
}

impl Drop for Sample<'_> {
    fn drop(&mut self) {
        self.record(None);
    }
}

fn write_report(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= MAX_BYTES,
        "Know Ops report exceeds size limit"
    );
    write_atomic(path, 0o600, Durability::Rename, |file| {
        file.write_all(bytes)?;
        Ok(())
    })
}

pub fn validate_report(bytes: &[u8]) -> Result<Value> {
    ensure!(
        bytes.len() <= MAX_BYTES,
        "Know Ops report exceeds size limit"
    );
    let report: Value = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("Know Ops report is invalid JSON"))?;
    ensure!(
        report["schema_version"].as_u64() == Some(VERSION),
        "unsupported Know Ops report schema"
    );
    ensure!(
        report["service"] == SERVICE && report["component"] == COMPONENT,
        "Know Ops report publisher mismatch"
    );
    ensure!(
        report["instance_id"]
            .as_str()
            .is_some_and(|id| Uuid::parse_str(id).is_ok()),
        "invalid Know Ops instance ID"
    );
    ensure!(
        report["sampled_at_unix_ms"]
            .as_u64()
            .is_some_and(|ms| ms > 0),
        "invalid Know Ops sample timestamp"
    );
    let health = &report["health"];
    ensure!(
        HealthState::deserialize(&health["state"]).is_ok()
            && HealthFreshness::deserialize(&health["freshness"]).is_ok()
            && health.get("reason").is_none_or(Value::is_string),
        "invalid Know Ops health"
    );
    let usage: Usage = serde_json::from_value(report["usage"].clone())
        .map_err(|_| anyhow::anyhow!("invalid Know Ops usage"))?;
    ensure!(
        usage.kb_search.valid() && usage.kb_read.valid(),
        "Know Ops counter exceeds the allowed range"
    );
    Ok(report)
}

pub fn default_inbox() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is required to locate the Know Ops inbox")?;
    Ok(PathBuf::from(home).join("Library/Application Support/know-ops/inbox"))
}

pub async fn relay(docker: &Path, context: &str, container: &str, inbox: &Path) -> Result<()> {
    ensure!(
        docker.is_absolute(),
        "Docker executable must be an absolute path"
    );
    if let Err(error) = fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(inbox)
        && error.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(error).context("could not create Know Ops inbox");
    }
    let metadata = fs::symlink_metadata(inbox)?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o777 == 0o700,
        "Know Ops inbox must be a real directory with mode 0700"
    );
    let lock = open_lock(&inbox.join(".know-things-daemon.lock"))?;
    lock.try_lock()
        .context("another Know Things relay is publishing")?;
    let bytes = timeout(READ_DEADLINE, async {
        let mut child = Command::new(docker)
            .args([
                "--context",
                context,
                "exec",
                "--",
                container,
                "cat",
                CONTAINER_SNAPSHOT,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("could not launch Docker to read Know Ops snapshot")?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .context("Docker stdout is unavailable")?
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() <= MAX_BYTES,
            "Know Ops report exceeds size limit"
        );
        ensure!(
            child.wait().await?.success(),
            "Docker could not read the Know Ops snapshot"
        );
        Ok::<_, anyhow::Error>(bytes)
    })
    .await
    .context("Docker Know Ops read exceeded five seconds")??;
    validate_report(&bytes)?;
    write_report(&inbox.join(INBOX_FILE), &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{ReadArgs, SearchArgs};
    use serde_json::json;

    #[test]
    fn diagnostic_latency_bounds_are_inclusive_and_count_overflow() {
        let mut measurements = Measurements::default();
        let hit = Response::Ok(json!({"status":"ok"}));
        for micros in [250, 251, 2_000_001] {
            measurements.record(Operation::Search, Some(&hit), Duration::from_micros(micros));
        }
        assert_eq!(
            measurements.search_latency,
            [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
        );
        assert_eq!(measurements.usage.kb_search.ok, 3);
        assert_eq!(measurements.search.hits, 3);
    }

    #[test]
    fn classifies_each_terminal_outcome_once_and_ignores_status() {
        let publisher = Publisher::new();
        let search = Request::Search(SearchArgs {
            query: "PrivateQuery".to_owned(),
            limit: None,
        });
        let read = Request::Read(ReadArgs {
            path: "private.md".to_owned(),
            commit: "private-revision".to_owned(),
            start_line: 1,
            end_line: 2,
        });
        assert!(publisher.track(&Request::Status).is_none());
        publisher
            .track(&search)
            .unwrap()
            .finish(&Response::Ok(json!({"status":"no_match"})));
        publisher
            .track(&search)
            .unwrap()
            .finish(&Response::error("invalid_arguments", "PrivateError"));
        publisher
            .track(&search)
            .unwrap()
            .finish(&Response::error("deadline_exceeded", "PrivateError"));
        drop(publisher.track(&search));
        publisher
            .track(&read)
            .unwrap()
            .finish(&Response::Ok(json!({"status":"stale_revision"})));
        publisher
            .track(&read)
            .unwrap()
            .finish(&Response::Ok(json!({"status":"unavailable"})));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        publisher
            .publish(
                &path,
                Health {
                    state: HealthState::Ready,
                    freshness: HealthFreshness::Unknown,
                    reason: None,
                },
                Details {
                    files: Some(0),
                    passages: None,
                    source_mode: SourceMode::Snapshot,
                },
            )
            .unwrap();
        let text = fs::read_to_string(path).unwrap();
        validate_report(text.as_bytes()).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        for key in ["ok", "rejected", "error", "cancelled"] {
            assert_eq!(value["usage"]["kb_search"][key], 1, "{key}");
        }
        assert_eq!(value["usage"]["kb_read"]["rejected"], 1);
        assert_eq!(value["usage"]["kb_read"]["error"], 1);
        assert_eq!(value["details"]["files"], 0);
        assert!(value["details"].get("passages").is_none());
        for sensitive in [
            "PrivateQuery",
            "PrivateError",
            "private.md",
            "private-revision",
        ] {
            assert!(!text.contains(sensitive));
        }
    }
}
