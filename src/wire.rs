use anyhow::{Context, Result, ensure};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};

pub const DEFAULT_SOCKET: &str = "/run/know-things/kb.sock";
pub const MAX_REQUEST: usize = 16 * 1024;
pub const MAX_RESPONSE: usize = 160 * 1024;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

pub fn retryable_accept(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::ConnectionAborted
    ) || matches!(
        error.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
    )
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    /// Question or exact identifier to look up in the KB.
    #[schemars(length(min = 1, max = 2048))]
    pub query: String,
    /// Maximum passages to return, between 1 and 10.
    #[schemars(range(min = 1, max = 10))]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    /// Repository-relative Markdown path from a search hit.
    pub path: String,
    /// Indexed snapshot revision from the hit's commit field.
    pub commit: String,
    /// First source line, inclusive.
    #[schemars(range(min = 1))]
    pub start_line: usize,
    /// Last source line, inclusive. A request can span up to 80 lines.
    #[schemars(range(min = 1))]
    pub end_line: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "tool", content = "args", rename_all = "snake_case")]
pub enum Request {
    Search(SearchArgs),
    Read(ReadArgs),
    Status,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Response {
    Ok(Value),
    Error(Failure),
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    Ok,
    NoMatch,
    Unavailable,
    StaleRevision,
    NotFound,
    RangeOutOfBounds,
}

impl ResultStatus {
    pub fn read(data: &Value) -> Option<Self> {
        Self::deserialize(&data["status"]).ok()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoMatchReason {
    InsufficientEvidence,
    NoRankedMatches,
}

impl NoMatchReason {
    pub fn read(data: &Value) -> Option<Self> {
        Self::deserialize(&data["reason"]).ok()
    }
}

#[derive(Deserialize, Serialize)]
pub struct Failure {
    pub code: String,
    pub message: String,
}

impl Response {
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error(Failure {
            code: code.to_owned(),
            message: message.into(),
        })
    }
}

pub async fn receive<T: DeserializeOwned>(
    reading: impl AsyncRead + Unpin,
    max: usize,
) -> Result<T> {
    let mut reader = BufReader::new(reading.take(max as u64 + 1));
    let mut bytes = Vec::new();
    reader.read_until(b'\n', &mut bytes).await?;
    ensure!(
        bytes.len() <= max && bytes.last() == Some(&b'\n'),
        "invalid RPC frame"
    );
    serde_json::from_slice(&bytes).context("invalid RPC message")
}

pub async fn send(
    writing: &mut (impl AsyncWrite + Unpin),
    value: &impl Serialize,
    max: usize,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() < max, "RPC frame exceeds size limit");
    bytes.push(b'\n');
    writing.write_all(&bytes).await?;
    Ok(())
}

pub async fn call(socket: &Path, request: Request) -> Result<Response> {
    timeout(REQUEST_TIMEOUT, async {
        let mut stream = UnixStream::connect(socket)
            .await
            .context("connect to local KB daemon")?;
        send(&mut stream, &request, MAX_REQUEST).await?;
        receive(&mut stream, MAX_RESPONSE).await
    })
    .await
    .context("local KB daemon timed out")?
}
