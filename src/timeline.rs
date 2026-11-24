use serde::{Deserialize, Serialize};
use std::{
    num::NonZeroU32,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Running,
    Succeeded,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Starting,
    Fetching,
    Exporting,
    WaitingForIndex,
    Indexing,
    Done,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Attempt {
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub outcome: Outcome,
    pub commit: Option<String>,
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_files: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_files: Option<usize>,
}

impl Attempt {
    pub fn running(commit: Option<String>) -> Self {
        Self {
            started_at_unix_ms: now_ms(),
            finished_at_unix_ms: None,
            duration_ms: None,
            outcome: Outcome::Running,
            commit,
            code: None,
            updated_files: None,
            removed_files: None,
        }
    }

    pub fn finish(&mut self, start: Instant, outcome: Outcome, code: Option<&str>) {
        self.finished_at_unix_ms = Some(now_ms());
        self.duration_ms = Some(start.elapsed().as_millis().try_into().unwrap_or(u64::MAX));
        self.outcome = outcome;
        self.code = code.map(str::to_owned);
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Stage {
    pub last_attempt: Option<Attempt>,
    pub last_success: Option<Attempt>,
}

impl Stage {
    pub fn record(&mut self, attempt: Attempt) {
        if attempt.outcome == Outcome::Succeeded {
            self.last_success = Some(attempt.clone());
        }
        self.last_attempt = Some(attempt);
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct SourceStages {
    pub managed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_interval_seconds: Option<NonZeroU32>,
    pub fetch: Stage,
    pub export: Stage,
}
