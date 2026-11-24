use crate::{
    timeline::{OperationPhase as Phase, Outcome as State},
    wire,
};
use anyhow::{Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::Semaphore,
    task::JoinSet,
    time::timeout,
};
use uuid::Uuid;

pub const SOCKET_NAME: &str = "admin.sock";
const DEFAULT_ADMIN_SOCKET: &str = "/run/know-things/admin.sock";
const FRAME_BYTES: usize = 16 * 1024;
const FRAME_DEADLINE: Duration = Duration::from_secs(1);

#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Queue index validation on the daemon refresh worker.
    Refresh {
        #[arg(long, default_value = DEFAULT_ADMIN_SOCKET)]
        socket: PathBuf,
        #[arg(long)]
        operation_id: Uuid,
        #[arg(long)]
        expected_commit: Option<String>,
    },
    /// Read an explicit daemon refresh outcome.
    Operation {
        #[arg(long, default_value = DEFAULT_ADMIN_SOCKET)]
        socket: PathBuf,
        #[arg(long)]
        operation_id: Uuid,
    },
    /// Read cached index and refresh state.
    Status {
        #[arg(long, default_value = DEFAULT_ADMIN_SOCKET)]
        socket: PathBuf,
    },
}

#[derive(Clone, Deserialize, Serialize)]
pub struct IndexProgress {
    pub(crate) phase: Phase,
    pub(crate) state: State,
    pub(crate) target_commit: Option<String>,
    pub(crate) indexed_commit: Option<String>,
    pub(crate) code: Option<String>,
}

impl IndexProgress {
    pub fn failed(code: &str, indexed_commit: Option<String>) -> Self {
        Self {
            phase: Phase::WaitingForIndex,
            state: State::Failed,
            target_commit: None,
            indexed_commit,
            code: Some(code.to_owned()),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Status,
    Refresh {
        operation_id: String,
        expected_commit: Option<String>,
    },
    Operation {
        operation_id: String,
    },
}

pub async fn serve(
    listener: UnixListener,
    handle: impl Fn(Request) -> Value + Send + Sync + 'static,
) -> Result<()> {
    let handle = Arc::new(handle);
    let slots = Arc::new(Semaphore::new(4));
    let mut active = JoinSet::new();
    loop {
        tokio::select! {
            _ = active.join_next(), if !active.is_empty() => {},
            incoming = listener.accept() => {
                let (mut stream, _) = match incoming {
                    Ok(connection) => connection,
                    Err(error) if wire::retryable_accept(&error) => {
                        eprintln!("KB admin socket temporarily unable to accept: {error}");
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
                let Ok(slot) = slots.clone().try_acquire_owned() else { continue };
                let handle = handle.clone();
                active.spawn(async move {
                    let _slot = slot;
                    let _ = timeout(FRAME_DEADLINE, async {
                        let request = wire::receive(&mut stream, FRAME_BYTES).await?;
                        let response = handle(request);
                        wire::send(&mut stream, &response, FRAME_BYTES).await
                    }).await;
                });
            }
        }
    }
}

pub async fn daemon_command(command: DaemonCommand) -> Result<Value> {
    let (socket, request) = match command {
        DaemonCommand::Status { socket } => (socket, Request::Status),
        DaemonCommand::Refresh {
            socket,
            operation_id,
            expected_commit,
        } => (
            socket,
            Request::Refresh {
                operation_id: operation_id.to_string(),
                expected_commit,
            },
        ),
        DaemonCommand::Operation {
            socket,
            operation_id,
        } => (
            socket,
            Request::Operation {
                operation_id: operation_id.to_string(),
            },
        ),
    };
    timeout(FRAME_DEADLINE, async {
        let mut stream = UnixStream::connect(socket)
            .await
            .context("connect to local KB admin socket")?;
        wire::send(&mut stream, &request, FRAME_BYTES).await?;
        wire::receive(&mut stream, FRAME_BYTES).await
    })
    .await
    .context("KB admin socket timed out")?
}
