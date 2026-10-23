use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CollectionPolicy {
    #[serde(default)]
    pub watch: bool,
    #[serde(default)]
    pub excluded_paths: Vec<String>,
    #[serde(default)]
    pub reconcile_interval_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<RetentionPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetentionPolicy {
    pub retain_revisions: usize,
    pub interval_ms: u64,
    #[serde(default = "default_retention_batch")]
    pub max_revisions: usize,
    #[serde(default = "default_retention_deadline")]
    pub deadline_ms: u64,
}
fn default_retention_batch() -> usize {
    128
}
fn default_retention_deadline() -> u64 {
    1000
}
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize, schemars::JsonSchema)]
#[error("{code}: {message}")]
#[serde(rename_all = "camelCase")]
pub struct Error {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}
impl Error {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            details: None,
        }
    }
}
impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(ref failure, _) = error {
            use rusqlite::ErrorCode::*;
            let (code, message, retryable) = match failure.code {
                OperationInterrupted => {
                    ("deadline_exceeded", "SQLite query deadline exceeded", false)
                }
                DiskFull => ("resource_exhausted", "Local storage is full", false),
                OutOfMemory => (
                    "resource_exhausted",
                    "SQLite memory allocation failed",
                    false,
                ),
                TooBig => (
                    "resource_exhausted",
                    "SQLite value exceeds storage limits",
                    false,
                ),
                DatabaseBusy | DatabaseLocked => ("busy", "Local storage is busy", true),
                DatabaseCorrupt | NotADatabase => (
                    "store_corrupt",
                    "Local storage is corrupt; run integrity diagnostics",
                    false,
                ),
                SystemIoFailure | CannotOpen | ReadOnly | PermissionDenied => {
                    ("io_error", "Local storage cannot be accessed", false)
                }
                _ => (
                    "store_error",
                    "SQLite operation failed; run integrity diagnostics",
                    false,
                ),
            };
            let mut error = Self::new(code, message);
            error.retryable = retryable;
            return error;
        }
        Self::new(
            "store_error",
            "SQLite operation failed; run integrity diagnostics",
        )
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::StorageFull || e.raw_os_error() == Some(libc::EDQUOT) {
            return Self::new("resource_exhausted", "Local storage capacity is exhausted");
        }
        if e.kind() == std::io::ErrorKind::OutOfMemory {
            return Self::new("resource_exhausted", "Local memory allocation failed");
        }
        if matches!(e.raw_os_error(), Some(libc::EMFILE | libc::ENFILE)) {
            return Self::new(
                "resource_exhausted",
                "Local file descriptor capacity is exhausted",
            );
        }
        Self::new("io_error", &format!("Local I/O failed ({:?})", e.kind()))
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::new("invalid_request", "Invalid JSON or operation fields")
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Lexical,
    Hybrid,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Search {
    pub query: String,
    pub collections: Vec<String>,
    #[serde(default = "default_k")]
    pub top_k: usize,
    #[serde(default = "default_budget")]
    pub evidence_token_budget: usize,
    #[serde(default = "default_deadline")]
    pub deadline_ms: u64,
    #[serde(default = "default_mode")]
    pub mode: Mode,
    #[serde(default)]
    pub strict_hybrid: bool,
}
fn default_k() -> usize {
    10
}
fn default_budget() -> usize {
    3000
}
fn default_deadline() -> u64 {
    5000
}
fn default_mode() -> Mode {
    Mode::Hybrid
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reference {
    pub collection_id: String,
    pub document_id: String,
    pub revision_id: String,
    pub chunk_id: String,
    pub content_hash: String,
    pub byte_start: usize,
    pub byte_end: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub reference: Reference,
    pub source: String,
    pub heading_path: String,
    pub text: String,
    pub token_count: usize,
    pub full_chunk: bool,
    pub continuation: bool,
    pub fusion_score: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EvidencePack {
    pub evidence_pack_id: String,
    pub corpus_generation: u64,
    pub actual_mode: Mode,
    pub embedding_space_id: Option<String>,
    pub degradation_reasons: Vec<String>,
    pub budget_tokenizer: String,
    pub token_count: usize,
    pub truncated: bool,
    pub evidence: Vec<Evidence>,
    pub timings_ms: BTreeMap<String, f64>,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextScope {
    #[default]
    Exact,
    Section,
    Neighbors,
    Document,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "operation",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Operation {
    Answer {
        #[serde(flatten)]
        request: Answer,
    },
    Cancel {
        run_id: String,
    },
    Register {
        name: String,
        root: String,
        #[serde(default)]
        policy: Option<CollectionPolicy>,
    },
    CollectionPolicy {
        collection: String,
    },
    SetCollectionPolicy {
        collection: String,
        policy: CollectionPolicy,
    },
    Reconcile {
        collection: String,
    },
    Search {
        #[serde(flatten)]
        request: Search,
    },
    Context {
        reference: Reference,
        #[serde(default)]
        scope: ContextScope,
        #[serde(default = "default_budget")]
        evidence_token_budget: usize,
    },
    Exclude {
        document: String,
    },
    Include {
        document: String,
    },
    RemoveCollection {
        collection: String,
    },
    Prune {
        retain_revisions: usize,
        #[serde(default)]
        max_revisions: Option<usize>,
        #[serde(default = "default_deadline")]
        deadline_ms: u64,
    },
    Status,
    Integrity,
    RebuildLexical {
        #[serde(default = "default_deadline")]
        deadline_ms: u64,
    },
    Backup {
        destination: String,
    },
}
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub schema_version: u32,
    pub request_id: String,
    #[serde(flatten)]
    pub operation: Operation,
}
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub schema_version: u32,
    pub request_id: String,
    pub status: String,
    pub duration_ms: f64,
    pub corpus_generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueuePolicy {
    #[default]
    Wait,
    Reject,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Answer {
    pub question: String,
    pub collections: Vec<String>,
    pub profile: String,
    #[serde(default = "default_budget")]
    pub evidence_token_budget: usize,
    #[serde(default = "default_input_budget")]
    pub input_token_budget: usize,
    #[serde(default = "default_output_budget")]
    pub output_token_budget: usize,
    #[serde(default = "default_answer_deadline")]
    pub deadline_ms: u64,
    #[serde(default)]
    pub queue_policy: QueuePolicy,
    #[serde(default = "default_answer_deadline")]
    pub queue_deadline_ms: u64,
    #[serde(default = "default_mode")]
    pub mode: Mode,
    #[serde(default)]
    pub strict_hybrid: bool,
}
fn default_input_budget() -> usize {
    4096
}
fn default_output_budget() -> usize {
    1024
}
fn default_answer_deadline() -> u64 {
    30000
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GenerationIdentity {
    pub profile_id: String,
    pub name: String,
    pub tokenizer_id: String,
    pub context_limit: usize,
    pub worker_build: String,
    pub flash_bundle_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Citation {
    pub label: String,
    pub reference: Reference,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AnswerResult {
    pub text: String,
    pub actual_mode: Mode,
    pub embedding_space_id: Option<String>,
    pub degradation_reasons: Vec<String>,
    pub evidence_truncated: bool,
    pub evidence_tokens: usize,
    pub visible_token_count_kind: String,
    pub evidence_pack_id: String,
    pub corpus_generation: u64,
    pub profile: GenerationIdentity,
    pub finish_reason: String,
    pub prompt_tokens: usize,
    pub generated_tokens: usize,
    pub visible_tokens: usize,
    pub hidden_tokens: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_prefix_tokens: Option<usize>,
    pub citations: Vec<Citation>,
    pub reference_validation: String,
    pub semantic_validation: String,
    pub timings_ms: BTreeMap<String, f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "event",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AnswerPayload {
    Accepted { queued: bool },
    EvidenceReady { pack: EvidencePack },
    AnswerDelta { text: String },
    Completed { answer: Box<AnswerResult> },
    Failed { error: Error, worker_stopped: bool },
}
impl AnswerPayload {
    pub fn terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed { .. })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AnswerEvent {
    pub run_id: String,
    pub sequence: u64,
    #[serde(flatten)]
    pub payload: AnswerPayload,
}
