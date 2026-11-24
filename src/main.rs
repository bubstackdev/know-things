mod admin;
mod console;
mod daemon;
mod evaluation;
mod index;
mod local_file;
mod markdown;
mod mcp;
mod refresh;
mod relevance;
mod shortcut;
mod source;
mod telemetry;
mod timeline;
mod wire;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::Serialize;
use serde_json::json;
use std::{
    future::Future,
    io::{self, Write},
    num::NonZeroU32,
    path::PathBuf,
    time::Duration,
};

pub const VERSION: &str = match option_env!("KNOW_THINGS_BUILD_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => concat!(env!("CARGO_PKG_VERSION"), "+local"),
};

#[derive(Parser)]
#[command(version = VERSION, about = "Local knowledge base retrieval")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Diagnose or operate a configured Know Things installation.
    Ops {
        #[command(subcommand)]
        command: console::HostCommand,
    },
    /// Access the daemon's private administration socket.
    Admin {
        #[command(subcommand)]
        command: admin::DaemonCommand,
    },
    /// Export a committed KB snapshot from a local Git repository.
    Export {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        source_dir: PathBuf,
        #[arg(long, default_value = "HEAD")]
        git_ref: String,
        /// HTTPS GitLab project URL used for citations.
        #[arg(long)]
        repo_url: String,
    },
    /// Fetch upstream main into a dedicated bare repository and export it on the host.
    Sync {
        #[arg(long)]
        mirror: PathBuf,
        #[arg(long)]
        source_dir: PathBuf,
        /// Git remote to fetch.
        #[arg(long)]
        remote: String,
        /// HTTPS GitLab project URL used for citations and to locate wikis.
        #[arg(long)]
        repo_url: String,
        /// Expected time between managed checks, as configured by the scheduler.
        #[arg(long)]
        check_interval_seconds: Option<NonZeroU32>,
    },
    /// Add the approved Shortcut Markdown export to a separate offline snapshot.
    ImportShortcut {
        #[arg(long)]
        base_dir: PathBuf,
        #[arg(long)]
        source_dir: PathBuf,
        #[arg(long)]
        docs: PathBuf,
        /// Shortcut workspace slug used for citations.
        #[arg(long)]
        workspace: String,
    },
    /// Build or update an offline index from an exported snapshot.
    Ingest {
        #[arg(long)]
        source_dir: PathBuf,
        #[arg(long)]
        database: PathBuf,
    },
    /// Query an index file directly for evaluation or diagnostics.
    Query {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        limit: Option<usize>,
        query: String,
    },
    /// Run the local KB daemon and refresh its index from the source directory.
    Serve {
        #[arg(long, default_value = "/source")]
        source_dir: PathBuf,
        #[arg(long, default_value = "/data/index.sqlite")]
        database: PathBuf,
        #[arg(long, default_value = wire::DEFAULT_SOCKET)]
        socket: PathBuf,
        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=3600))]
        poll_seconds: u64,
    },
    /// Serve MCP over stdin and stdout using the running daemon.
    Mcp {
        #[arg(long, default_value = wire::DEFAULT_SOCKET)]
        socket: PathBuf,
    },
    /// Copy the current daemon report into the local Know Ops inbox.
    RelayOps {
        #[arg(long)]
        docker: PathBuf,
        #[arg(long, default_value = "desktop-linux")]
        context: String,
        #[arg(long, default_value = "know-things")]
        container: String,
        #[arg(long)]
        inbox: Option<PathBuf>,
    },
    /// Inspect coverage and freshness from the running daemon.
    Status {
        #[arg(long, default_value = wire::DEFAULT_SOCKET)]
        socket: PathBuf,
        /// Exit with failure when no indexed snapshot is available.
        #[arg(long)]
        ready: bool,
    },
    /// Search through the running daemon from the local CLI.
    Search {
        #[arg(long, default_value = wire::DEFAULT_SOCKET)]
        socket: PathBuf,
        #[arg(long)]
        limit: Option<usize>,
        query: String,
    },
    /// Score a labeled, pinned question set and time warm daemon calls.
    Evaluate {
        #[arg(long, default_value = wire::DEFAULT_SOCKET)]
        socket: PathBuf,
        /// JSONL question set, or - to read it from standard input.
        #[arg(long)]
        questions: PathBuf,
        #[arg(long)]
        commit: String,
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u16).range(1..=100))]
        repetitions: u16,
    },
}

fn print_json(value: impl Serialize) -> Result<()> {
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, &value)?;
    writeln!(output)?;
    Ok(())
}

fn run_async<T>(job: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(daemon::BLOCKING_WORKERS)
        .thread_keep_alive(Duration::from_secs(365 * 24 * 3600))
        .enable_all()
        .build()?
        .block_on(job)
}

fn call(socket: &std::path::Path, request: wire::Request) -> Result<serde_json::Value> {
    match run_async(wire::call(socket, request))? {
        wire::Response::Ok(data) => Ok(data),
        wire::Response::Error(error) => anyhow::bail!("{}: {}", error.code, error.message),
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Ops { command } => {
            let reply = run_async(console::host_command(command))?;
            print_json(reply.value)?;
            anyhow::ensure!(reply.success, "Know Things console action did not succeed");
            Ok(())
        }
        Command::Admin { command } => print_json(run_async(admin::daemon_command(command))?),
        Command::Export {
            repo,
            source_dir,
            git_ref,
            repo_url,
        } => {
            let snapshot = source::export(&repo, &source_dir, &git_ref, &repo_url)?;
            print_json(json!({ "commit": snapshot.commit, "files": snapshot.files.len() }))
        }
        Command::Sync {
            mirror,
            source_dir,
            remote,
            repo_url,
            check_interval_seconds,
        } => print_json(source::sync(
            &mirror,
            &source_dir,
            &remote,
            &repo_url,
            check_interval_seconds,
        )?),
        Command::ImportShortcut {
            base_dir,
            source_dir,
            docs,
            workspace,
        } => {
            let snapshot = source::import_shortcut(&base_dir, &source_dir, &docs, &workspace)?;
            print_json(json!({
                "commit": snapshot.revision(),
                "shortcut_docs": snapshot.shortcut_docs.len(),
            }))
        }
        Command::Ingest {
            source_dir,
            database,
        } => {
            let snapshot = source::Snapshot::load(&source_dir)?;
            print_json(index::ingest(
                &mut index::initialize(&database)?,
                &snapshot,
            )?)
        }
        Command::Query {
            database,
            limit,
            query,
        } => print_json(index::search(
            &mut index::reader(&database, None)?,
            &query,
            limit,
        )?),
        Command::Serve {
            source_dir,
            database,
            socket,
            poll_seconds,
        } => run_async(daemon::serve(
            database,
            source_dir,
            socket,
            Duration::from_secs(poll_seconds),
        )),
        Command::Mcp { socket } => run_async(mcp::serve(socket)),
        Command::RelayOps {
            docker,
            context,
            container,
            inbox,
        } => {
            let inbox = match inbox {
                Some(path) => path,
                None => telemetry::default_inbox()?,
            };
            run_async(telemetry::relay(&docker, &context, &container, &inbox))
        }
        Command::Status { socket, ready } => {
            let status = call(&socket, wire::Request::Status)?;
            let available = status["indexed_commit"].is_string();
            print_json(status)?;
            anyhow::ensure!(
                !ready || available,
                "KB is waiting for an indexed source snapshot"
            );
            Ok(())
        }
        Command::Search {
            socket,
            limit,
            query,
        } => print_json(call(
            &socket,
            wire::Request::Search(wire::SearchArgs { query, limit }),
        )?),
        Command::Evaluate {
            socket,
            questions,
            commit,
            repetitions,
        } => print_json(run_async(evaluation::evaluate(
            &socket,
            &questions,
            &commit,
            repetitions,
        ))?),
    }
}
