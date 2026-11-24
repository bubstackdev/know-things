mod docker;
mod operation;

use crate::{local_file, source::SourceMode, telemetry, timeline};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use docker::Docker;
use operation::Action;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const MAX_JSON_BYTES: usize = 16 * 1024;
const RELAY_MAX_AGE_MS: u64 = 90_000;
const RELAY_MAX_SKEW_MS: u64 = 30_000;

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    local_file::read_json(path, MAX_JSON_BYTES as u64)
}

#[derive(Subcommand)]
pub enum HostCommand {
    /// Refresh the configured source and make it searchable.
    Refresh {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        operation_id: Uuid,
    },
    /// Revalidate and index the current export.
    RetryIndex {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        operation_id: Uuid,
    },
    /// Read a retained console operation.
    Operation {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        operation_id: Uuid,
    },
    /// Describe the configured container and its KB index.
    Diagnose {
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Source {
    Snapshot {
        checkout: PathBuf,
        git_ref: String,
        repo_url: String,
        source_dir: PathBuf,
    },
    Managed {
        mirror: PathBuf,
        remote: String,
        repo_url: String,
        source_dir: PathBuf,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    schema_version: u32,
    docker: Docker,
    state_dir: PathBuf,
    relay_inbox: Option<PathBuf>,
    source: Source,
}

impl Profile {
    fn load(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "console profile path must be absolute");
        let profile: Self = read_json(path).context("invalid console profile")?;
        ensure!(
            profile.schema_version == 1,
            "unsupported console profile schema"
        );
        ensure!(
            profile.docker.executable.is_absolute()
                && profile.state_dir.is_absolute()
                && profile
                    .relay_inbox
                    .as_ref()
                    .is_none_or(|path| path.is_absolute()),
            "console profile paths must be absolute"
        );
        ensure!(
            !profile.docker.container.is_empty() && !profile.docker.context.is_empty(),
            "console profile requires a Docker context and container"
        );
        let (repo, reference, repo_url, dir) = match &profile.source {
            Source::Snapshot {
                checkout,
                git_ref,
                repo_url,
                source_dir,
            } => (checkout, git_ref, repo_url, source_dir),
            Source::Managed {
                mirror,
                remote,
                repo_url,
                source_dir,
            } => (mirror, remote, repo_url, source_dir),
        };
        ensure!(
            repo.is_absolute() && dir.is_absolute(),
            "source paths must be absolute"
        );
        ensure!(
            !reference.is_empty(),
            "source Git ref or remote must be nonempty"
        );
        crate::source::repo_url(repo_url)?;
        Ok(profile)
    }
}

pub struct HostReply {
    pub value: Value,
    pub success: bool,
}

pub async fn host_command(command: HostCommand) -> Result<HostReply> {
    let (value, success) = match command {
        HostCommand::Diagnose { config } => (diagnose(&Profile::load(&config)?).await, true),
        HostCommand::Operation {
            config,
            operation_id,
        } => {
            let operation = operation::load(&Profile::load(&config)?, operation_id).await?;
            (serde_json::to_value(operation)?, true)
        }
        HostCommand::Refresh {
            config,
            operation_id,
        } => action(&config, operation_id, Action::Refresh).await?,
        HostCommand::RetryIndex {
            config,
            operation_id,
        } => action(&config, operation_id, Action::RetryIndex).await?,
    };
    Ok(HostReply { value, success })
}

async fn action(config: &Path, id: Uuid, action: Action) -> Result<(Value, bool)> {
    let operation = operation::run_action(&Profile::load(config)?, id, action).await?;
    let success = operation.succeeded();
    Ok((serde_json::to_value(operation)?, success))
}

fn relay_observation(profile: &Profile) -> Value {
    let Ok(default_inbox) = profile
        .relay_inbox
        .clone()
        .map_or_else(telemetry::default_inbox, Ok)
    else {
        return json!({"state":"invalid", "sampled_at_unix_ms":null});
    };
    let value =
        match local_file::read_bytes(&default_inbox.join(telemetry::INBOX_FILE), MAX_JSON_BYTES)
            .and_then(|bytes| telemetry::validate_report(&bytes))
        {
            Ok(value) => value,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|err| err.kind() == ErrorKind::NotFound) =>
            {
                return json!({"state":"missing", "sampled_at_unix_ms":null});
            }
            Err(_) => return json!({"state":"invalid", "sampled_at_unix_ms":null}),
        };
    let Some(sampled) = value["sampled_at_unix_ms"]
        .as_u64()
        .filter(|sampled| *sampled > 0)
    else {
        return json!({"state":"invalid", "sampled_at_unix_ms":null});
    };
    let now = timeline::now_ms();
    if sampled > now.saturating_add(RELAY_MAX_SKEW_MS) {
        return json!({"state":"invalid", "sampled_at_unix_ms":null});
    }
    let state = if now.saturating_sub(sampled) > RELAY_MAX_AGE_MS {
        "stale"
    } else {
        "fresh"
    };
    json!({"state":state, "sampled_at_unix_ms":sampled})
}

async fn diagnose(profile: &Profile) -> Value {
    let (container, code) = docker::container_state(&profile.docker).await;
    let daemon = if container == docker::ContainerState::Running {
        docker::daemon_observation(&profile.docker).await
    } else {
        json!({"state":"unknown", "code":null, "sample":null})
    };
    json!({
        "schema_version": 1,
        "sampled_at_unix_ms": timeline::now_ms(),
        "source_mode": match profile.source {
            Source::Snapshot { .. } => SourceMode::Snapshot,
            Source::Managed { .. } => SourceMode::Managed,
        },
        "container": {"state":container, "code":code},
        "relay": relay_observation(profile),
        "daemon": daemon,
    })
}
