use super::MAX_JSON_BYTES;
use crate::admin::IndexProgress;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

const DOCKER_DEADLINE: Duration = Duration::from_secs(5);
const INSIDE_BINARY: &str = "/usr/local/bin/know-things";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Docker {
    pub(super) executable: PathBuf,
    pub(super) context: String,
    pub(super) container: String,
}

pub async fn index_progress(docker: &Docker, args: &[&str]) -> Result<IndexProgress> {
    let reply = inside(docker, args).await?;
    ensure!(
        reply.status.success(),
        "KB admin command exits unsuccessfully"
    );
    Ok(serde_json::from_slice(&reply.stdout)?)
}

pub async fn indexed_commit(docker: &Docker) -> Option<String> {
    let reply = inside(docker, &["admin", "status"]).await.ok()?;
    if !reply.status.success() {
        return None;
    }
    let sample = serde_json::from_slice::<Value>(&reply.stdout).ok()?;
    sample["indexed_commit"].as_str().map(str::to_owned)
}

struct DockerOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn docker_output(profile: &Docker, args: &[&str]) -> Result<DockerOutput> {
    timeout(DOCKER_DEADLINE, async {
        let mut child = Command::new(&profile.executable)
            .args(["--context", &profile.context])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = child
            .stdout
            .take()
            .context("missing Docker stdout")?
            .take(MAX_JSON_BYTES as u64 + 1);
        let mut err = child
            .stderr
            .take()
            .context("missing Docker stderr")?
            .take(MAX_JSON_BYTES as u64 + 1);
        tokio::try_join!(out.read_to_end(&mut stdout), err.read_to_end(&mut stderr))?;
        ensure!(
            stdout.len() <= MAX_JSON_BYTES && stderr.len() <= MAX_JSON_BYTES,
            "Docker output exceeds size limit"
        );
        let status = child.wait().await?;
        Ok(DockerOutput {
            status,
            stdout,
            stderr,
        })
    })
    .await
    .context("Docker command timed out")?
}

#[derive(PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ContainerState {
    Running,
    Stopped,
    DockerUnreachable,
    Unknown,
    Missing,
}

pub(super) async fn container_state(docker: &Docker) -> (ContainerState, Option<&'static str>) {
    let inspect = docker_output(
        docker,
        &[
            "container",
            "inspect",
            "--format",
            "{{json .State}}",
            "--",
            &docker.container,
        ],
    )
    .await;
    let Ok(inspect) = inspect else {
        return (
            ContainerState::DockerUnreachable,
            Some("docker_unreachable"),
        );
    };
    if inspect.status.success() {
        return match serde_json::from_slice::<Value>(&inspect.stdout)
            .ok()
            .and_then(|value| value["Running"].as_bool())
        {
            Some(true) => (ContainerState::Running, None),
            Some(false) => (ContainerState::Stopped, Some("container_stopped")),
            None => (ContainerState::Unknown, Some("container_state_invalid")),
        };
    }
    if !docker_output(docker, &["info", "--format", "{{json .ServerVersion}}"])
        .await
        .is_ok_and(|info| info.status.success())
    {
        return (
            ContainerState::DockerUnreachable,
            Some("docker_unreachable"),
        );
    }
    let message = String::from_utf8_lossy(&inspect.stderr);
    if message.contains("No such container:") || message.contains("No such object:") {
        (ContainerState::Missing, Some("container_missing"))
    } else {
        (ContainerState::Unknown, Some("container_inspect_failed"))
    }
}

async fn inside(docker: &Docker, args: &[&str]) -> Result<DockerOutput> {
    let mut command = vec!["exec", "--", &docker.container, INSIDE_BINARY];
    command.extend_from_slice(args);
    docker_output(docker, &command).await
}

pub(super) async fn daemon_observation(docker: &Docker) -> Value {
    let Ok(admin) = inside(docker, &["admin", "status"]).await else {
        return json!({"state":"unreachable", "code":"console_transport_failed", "sample":null});
    };
    if admin.status.success() {
        if let Ok(sample) = serde_json::from_slice::<Value>(&admin.stdout)
            && sample["instance_id"].as_str().is_some()
            && sample["sampled_at_unix_ms"].as_u64().is_some()
        {
            return json!({"state":"reachable", "code":null, "sample":sample});
        }
        return json!({"state":"unreachable", "code":"console_response_invalid", "sample":null});
    }
    if admin.status.code() == Some(2)
        && inside(docker, &["status"]).await.is_ok_and(|legacy| {
            legacy.status.success()
                && serde_json::from_slice::<Value>(&legacy.stdout)
                    .is_ok_and(|status| status.is_object())
        })
    {
        return json!({"state":"unsupported", "code":"unsupported_console_api", "sample":null});
    }
    json!({"state":"unreachable", "code":"admin_unreachable", "sample":null})
}
