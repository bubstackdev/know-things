use crate::{
    local_file::{self, read_json},
    shortcut,
    timeline::{Attempt, Outcome, SourceStages},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha1::Digest;
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    num::NonZeroU32,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

pub const SNAPSHOT_FILE: &str = "snapshot.json";
pub const SYNC_FILE: &str = "sync.json";
pub const OPERATION_ENV: &str = "KNOW_THINGS_CONSOLE_OPERATION_ID";
pub const SOURCE_BUSY: &str = "source export is busy";
const STAGES_FILE: &str = "source-observations.json";
const MAX_STAGE_BYTES: u64 = 8 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_MARKDOWN: usize = 32 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 512 * 1024;
pub const MAX_LINE_BYTES: usize = 8 * 1024;
const MAX_FILES: usize = 10_000;
const WIKI_CONFIG: &str = "wiki-sources.json";
const MAX_WIKIS: usize = 32;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceMode {
    Managed,
    Snapshot,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub version: u32,
    pub commit: String,
    pub repo_url: String,
    pub files: Vec<Document>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wikis: Vec<WikiSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shortcut_docs: Vec<shortcut::SnapshotDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortcut_workspace: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub path: String,
    pub blob: String,
    pub markdown: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
enum WikiScope {
    Group,
    Project,
}

impl WikiScope {
    fn name(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WikiTarget {
    scope: WikiScope,
    owner: String,
    paths: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WikiSnapshot {
    scope: WikiScope,
    owner: String,
    pub commit: String,
    pub files: Vec<Document>,
}

impl WikiSnapshot {
    pub fn path(&self, path: &str) -> String {
        format!("gitlab-wikis/{}/{}/{path}", self.scope.name(), self.owner)
    }

    pub fn citation(&self, repo: &Url, path: &str) -> Result<String> {
        let mut url = repo.clone();
        let slug = path
            .rsplit_once('.')
            .context("wiki path is missing an extension")?
            .0;
        {
            let mut segments = url.path_segments_mut().expect("GitLab URL supports paths");
            segments.clear();
            if self.scope == WikiScope::Group {
                segments.push("groups");
            }
            segments
                .extend(self.owner.split('/'))
                .extend(["-", "wikis"]);
            segments.extend(slug.split('/'));
        }
        url.query_pairs_mut()
            .append_pair("version_id", &self.commit);
        Ok(url.into())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SyncStatus {
    pub managed: bool,
    pub upstream_commit: Option<String>,
    pub checked_at_unix: Option<u64>,
    pub last_error: Option<String>,
    // Read early cadence exports, but keep the on-disk record readable by deployed daemons.
    #[serde(skip_serializing)]
    pub check_interval_seconds: Option<NonZeroU32>,
}

#[derive(Serialize)]
pub struct SyncReport {
    #[serde(flatten)]
    status: SyncStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    check_interval_seconds: Option<NonZeroU32>,
}

impl Snapshot {
    pub fn load(dir: &Path) -> Result<Self> {
        let snapshot: Self = read_json(&dir.join(SNAPSHOT_FILE), MAX_SNAPSHOT_BYTES)?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn revision(&self) -> String {
        if self.wikis.is_empty() && self.shortcut_docs.is_empty() {
            return self.commit.clone();
        }
        let mut hash = sha2::Sha256::new();
        hash.update(b"know-things corpus v1\0");
        hash.update(self.commit.as_bytes());
        for wiki in &self.wikis {
            hash.update([0]);
            hash.update(wiki.scope.name().as_bytes());
            hash.update([0]);
            hash.update(wiki.owner.as_bytes());
            hash.update([0]);
            hash.update(wiki.commit.as_bytes());
            for doc in &wiki.files {
                hash.update([0]);
                hash.update(doc.path.as_bytes());
                hash.update([0]);
                hash.update(doc.blob.as_bytes());
            }
        }
        if !self.shortcut_docs.is_empty() {
            hash.update(b"\0shortcut-docs");
            if let Some(workspace) = &self.shortcut_workspace {
                hash.update(b"\0shortcut-workspace\0");
                hash.update(workspace.as_bytes());
            }
        }
        for page in &self.shortcut_docs {
            for value in [
                &page.doc.id,
                &page.doc.title,
                &page.doc.updated_at,
                &page.sha256,
            ] {
                hash.update([0]);
                hash.update(value.as_bytes());
            }
        }
        format!("{:x}", hash.finalize())
    }

    fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported snapshot format");
        ensure!(is_oid(&self.commit), "invalid commit ID");
        repo_url(&self.repo_url)?;
        ensure!(self.wikis.len() <= MAX_WIKIS, "too many GitLab wikis");
        let mut wiki_ids = HashSet::new();
        for wiki in &self.wikis {
            validate_wiki_owner(&wiki.owner)?;
            ensure!(is_oid(&wiki.commit), "invalid wiki commit ID");
            ensure!(
                wiki_ids.insert((wiki.scope, wiki.owner.to_ascii_lowercase())),
                "duplicate GitLab wiki"
            );
        }
        let mut seen = HashSet::new();
        let mut total = 0;
        let primary = self
            .files
            .iter()
            .map(|doc| (doc, doc.path.clone(), &self.commit));
        let wiki_pages = self.wikis.iter().flat_map(|wiki| {
            wiki.files
                .iter()
                .map(|doc| (doc, wiki.path(&doc.path), &wiki.commit))
        });
        let mut validate_markdown = |path: String, markdown: &str| {
            ensure!(is_markdown_path(&path), "invalid Markdown path");
            ensure!(seen.insert(path.clone()), "duplicate Markdown path: {path}");
            ensure!(seen.len() <= MAX_FILES, "too many Markdown files");
            ensure!(
                markdown.len() <= MAX_FILE_BYTES
                    && !markdown.contains('\0')
                    && markdown.lines().all(|line| line.len() < MAX_LINE_BYTES),
                "unsupported Markdown size or content in {path}"
            );
            total += markdown.len();
            ensure!(
                total <= MAX_TOTAL_MARKDOWN,
                "total Markdown size exceeds limit"
            );
            Ok(())
        };
        for (doc, path, commit) in primary.chain(wiki_pages) {
            ensure!(is_markdown_path(&doc.path), "invalid Markdown path");
            validate_markdown(path, &doc.markdown)?;
            ensure!(
                is_oid(&doc.blob) && doc.blob.len() == commit.len(),
                "invalid blob ID in {}",
                doc.path
            );
            ensure!(
                doc.blob == blob_id(doc.markdown.as_bytes(), doc.blob.len()),
                "blob content does not match Git object ID in {}",
                doc.path
            );
        }
        if let Some(workspace) = &self.shortcut_workspace {
            shortcut::validate_workspace(workspace)?;
        }
        for page in &self.shortcut_docs {
            validate_markdown(page.path(), &page.doc.content_markdown)?;
            page.validate()?;
        }
        Ok(())
    }
}

fn validate_wiki_owner(owner: &str) -> Result<()> {
    ensure!(
        owner.len() <= 512
            && owner.split('/').all(|part| !part.is_empty()
                && !matches!(part, "." | "..")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))),
        "invalid GitLab wiki owner path"
    );
    Ok(())
}

fn wiki_targets(dir: &Path) -> Result<Vec<WikiTarget>> {
    let path = dir
        .parent()
        .context("source directory needs a private parent")?
        .join(WIKI_CONFIG);
    let mut targets: Vec<WikiTarget> = match read_json(&path, 64 * 1024) {
        Ok(targets) => targets,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error).context("could not load GitLab wiki sources"),
    };
    ensure!(targets.len() <= MAX_WIKIS, "too many GitLab wiki sources");
    let mut ids = HashSet::new();
    for target in &targets {
        validate_wiki_owner(&target.owner)?;
        ensure!(
            ids.insert((target.scope, target.owner.to_ascii_lowercase())),
            "duplicate GitLab wiki source"
        );
        ensure!(
            !target.paths.is_empty(),
            "each GitLab wiki needs selected Markdown paths"
        );
        for path in &target.paths {
            let valid = match path.strip_suffix('/') {
                Some(directory) => is_markdown_path(&format!("{directory}/page.md")),
                None => is_markdown_path(path),
            };
            ensure!(valid, "invalid GitLab wiki path selector");
        }
    }
    targets.sort_by(|a, b| (a.scope, &a.owner).cmp(&(b.scope, &b.owner)));
    Ok(targets)
}

pub fn repo_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("invalid repository URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.path().trim_matches('/').contains('/')
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "repository URL must be an HTTPS project URL without credentials"
    );
    Ok(url)
}

pub fn is_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub fn is_markdown_path(path: &str) -> bool {
    path.len() <= 1024
        && Path::new(path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| {
            !part.is_empty() && !matches!(part, "." | "..") && !part.eq_ignore_ascii_case(".git")
        })
}

fn blob_id(bytes: &[u8], oid_len: usize) -> String {
    let header = format!("blob {}\0", bytes.len());
    if oid_len == 40 {
        let mut hash = sha1::Sha1::new();
        hash.update(header);
        hash.update(bytes);
        format!("{:x}", hash.finalize())
    } else {
        let mut hash = sha2::Sha256::new();
        hash.update(header);
        hash.update(bytes);
        format!("{:x}", hash.finalize())
    }
}

fn git(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1");
    command
}

fn git_stdout(command: &mut Command, operation: &str) -> Result<Vec<u8>> {
    let output = command
        .output()
        .with_context(|| format!("could not start Git for {operation}"))?;
    if !output.status.success() {
        let stderr = &output.stderr[output.stderr.len().saturating_sub(2048)..];
        let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
        let (code, hint) = if text.contains("host key verification failed")
            || text.contains("remote host identification has changed")
        {
            (
                "git_host_key_failed",
                "SSH host key verification failed. Check the host's known_hosts file.",
            )
        } else if text.contains("permission denied (publickey")
            || text.contains("authentication failed")
        {
            (
                "git_auth_failed",
                "Git authentication failed. Check the credentials available to the sync process.",
            )
        } else if text.contains("could not resolve") {
            ("git_dns_failed", "Git could not resolve the remote host.")
        } else if text.contains("connection timed out") || text.contains("operation timed out") {
            (
                "git_connection_timeout",
                "The connection to the Git remote timed out.",
            )
        } else if text.contains("repository not found")
            || text.contains("does not appear to be a git repository")
        {
            (
                "git_remote_unavailable",
                "The Git repository is unavailable to the sync process.",
            )
        } else if text.contains("couldn't find remote ref") {
            (
                "git_ref_missing",
                "The requested Git reference is missing from the remote.",
            )
        } else {
            (
                "sync_failed",
                "Check the configured Git source and host connectivity.",
            )
        };
        return Err(anyhow::Error::new(GitFailure { code }).context(format!(
            "Git {operation} failed ({}). {hint}",
            output.status
        )));
    }
    Ok(output.stdout)
}

#[derive(Debug)]
struct GitFailure {
    code: &'static str,
}

impl std::fmt::Display for GitFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code)
    }
}

impl std::error::Error for GitFailure {}

fn collect(repo: &Path, reference: &str, url: &str, paths: &[String]) -> Result<Snapshot> {
    let commit = String::from_utf8(git_stdout(
        git(repo).args([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ]),
        "resolve commit",
    )?)?
    .trim()
    .to_owned();
    ensure!(is_oid(&commit), "Git returned an invalid commit");
    let tree = git_stdout(
        git(repo).args(["ls-tree", "-rlz", "--full-tree", &commit]),
        "read commit tree",
    )?;
    let mut entries = Vec::new();
    let mut skipped = 0;
    for record in tree
        .split(|&byte| byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|&byte| byte == b'\t')
            .context("invalid Git tree entry")?;
        let path = &record[tab + 1..];
        if !path
            .get(path.len().saturating_sub(3)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(b".md"))
        {
            continue;
        }
        if !paths.is_empty()
            && !paths.iter().any(|selector| {
                if selector.ends_with('/') {
                    path.starts_with(selector.as_bytes())
                } else {
                    path == selector.as_bytes()
                }
            })
        {
            continue;
        }
        let Ok(path) = std::str::from_utf8(path) else {
            skipped += 1;
            continue;
        };
        if !is_markdown_path(path)
            || paths.is_empty()
                && (path.starts_with("gitlab-wikis/") || path.starts_with(shortcut::PATH_PREFIX))
        {
            skipped += 1;
            continue;
        }
        let meta = std::str::from_utf8(&record[..tab])?;
        let mut parts = meta.split_whitespace();
        let mode = parts.next();
        let kind = parts.next();
        let oid = parts.next().context("invalid Git tree entry")?;
        let size = parts.next().and_then(|size| size.parse::<usize>().ok());
        if !matches!(mode, Some("100644" | "100755"))
            || kind != Some("blob")
            || size.is_none_or(|size| size > MAX_FILE_BYTES)
        {
            skipped += 1;
            continue;
        }
        ensure!(is_oid(oid), "invalid Git tree object ID");
        ensure!(entries.len() < MAX_FILES, "too many Markdown files");
        entries.push((path, oid));
    }

    let mut child = git(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start Git blob reader")?;
    let files = (|| {
        let mut input = child.stdin.take().context("missing Git stdin")?;
        let mut output = BufReader::new(child.stdout.take().context("missing Git stdout")?);
        let mut files = Vec::with_capacity(entries.len());
        let mut total = 0;
        for (path, oid) in entries {
            writeln!(input, "{oid}")?;
            let mut header = String::new();
            output.read_line(&mut header)?;
            let size: usize = header
                .trim_end()
                .strip_prefix(&format!("{oid} blob "))
                .context("could not read Git blob header")?
                .parse()?;
            ensure!(
                size <= MAX_FILE_BYTES,
                "Markdown size limit exceeded at {path}"
            );
            let mut content = vec![0; size];
            output.read_exact(&mut content)?;
            let mut delimiter = [0];
            output.read_exact(&mut delimiter)?;
            ensure!(delimiter == *b"\n", "invalid Git blob terminator");
            let Ok(markdown) = String::from_utf8(content) else {
                skipped += 1;
                continue;
            };
            if markdown.contains('\0') || markdown.lines().any(|line| line.len() >= MAX_LINE_BYTES)
            {
                skipped += 1;
                continue;
            }
            total += size;
            ensure!(
                total <= MAX_TOTAL_MARKDOWN,
                "total Markdown size limit exceeded at {path}"
            );
            files.push(Document {
                path: path.to_owned(),
                blob: oid.to_owned(),
                markdown,
            });
        }
        Ok::<_, anyhow::Error>(files)
    })();
    let status = child.wait()?;
    if skipped > 0 {
        eprintln!("KB export skipped {skipped} unsupported Markdown entries at {commit}");
    }
    let snapshot = Snapshot {
        version: 1,
        commit,
        repo_url: url.to_owned(),
        files: files?,
        wikis: Vec::new(),
        shortcut_docs: Vec::new(),
        shortcut_workspace: None,
    };
    ensure!(status.success(), "Git blob reader failed");
    snapshot.validate()?;
    Ok(snapshot)
}

fn export_lock(dir: &Path) -> Result<File> {
    let parent = dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("source-dir must be a child of a private directory")?;
    ensure!(
        fs::metadata(parent)
            .context("create the source parent directory first")?
            .permissions()
            .mode()
            & 0o077
            == 0,
        "source-dir parent must be private (chmod 700); Docker reads the exported child directory"
    );
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o755);
    match builder.create(dir) {
        Ok(()) => fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(dir)?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o022 == 0,
        "source-dir must be a real directory without group or world write access"
    );
    let file = local_file::open_lock(&dir.join("export.lock"))?;
    if operation_id().is_some() {
        try_lock_source(&file)?;
    } else {
        file.lock().context("could not lock source export")?;
    }

    Ok(file)
}

fn operation_id() -> Option<String> {
    std::env::var(OPERATION_ENV)
        .ok()
        .and_then(|id| uuid::Uuid::parse_str(&id).ok())
        .map(|id| id.to_string())
}

fn try_lock_source(file: &File) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(fs::TryLockError::WouldBlock) => {
                ensure!(Instant::now() < deadline, "{SOURCE_BUSY}");
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error).context("could not lock source export"),
        }
    }
}

pub fn interrupt_stages(dir: &Path, id: &str, code: &str) {
    let Ok(file) = OpenOptions::new().write(true).open(dir.join("export.lock")) else {
        return;
    };
    if file.try_lock().is_err() {
        return;
    }
    let Ok(mut stages) = load_stages(dir) else {
        return;
    };
    if stages.operation_id.as_deref() != Some(id) {
        return;
    }
    let stage = [&mut stages.fetch, &mut stages.export]
        .into_iter()
        .find(|stage| {
            stage
                .last_attempt
                .as_ref()
                .is_some_and(|attempt| attempt.outcome == Outcome::Running)
        });
    if let Some(stage) = stage {
        let attempt = stage.last_attempt.as_mut().unwrap();
        attempt.outcome = Outcome::Failed;
        attempt.finished_at_unix_ms = Some(crate::timeline::now_ms());
        // The source process owned the monotonic timer.
        attempt.code = Some(code.to_owned());
        let _ = write_json(dir, STAGES_FILE, &stages, MAX_STAGE_BYTES);
    }
}

pub fn export(repo: &Path, dir: &Path, reference: &str, url: &str) -> Result<Snapshot> {
    let _lock = export_lock(dir)?;
    let mut stages = load_stages(dir)
        .ok()
        .filter(|stages| !stages.managed)
        .unwrap_or_default();
    stages.operation_id = operation_id();
    let result = observe(dir, &mut stages, false, || {
        let snapshot = collect(repo, reference, url, &[])?;
        write_json(dir, SNAPSHOT_FILE, &snapshot, MAX_SNAPSHOT_BYTES)?;
        write_json(dir, SYNC_FILE, &SyncStatus::default(), 4096)?;
        let commit = snapshot.commit.clone();
        Ok((snapshot, Some(commit), None))
    })?;
    Ok(result)
}

pub fn import_shortcut(base: &Path, dir: &Path, docs: &Path, workspace: &str) -> Result<Snapshot> {
    shortcut::validate_workspace(workspace)?;
    let mut snapshot = Snapshot::load(base)?;
    let pages: Vec<shortcut::Doc> = read_json(docs, MAX_SNAPSHOT_BYTES)?;
    snapshot.shortcut_docs = pages.into_iter().map(shortcut::SnapshotDoc::new).collect();
    snapshot.shortcut_workspace =
        (!snapshot.shortcut_docs.is_empty()).then(|| workspace.to_owned());
    snapshot
        .shortcut_docs
        .sort_unstable_by(|a, b| a.doc.id.cmp(&b.doc.id));
    snapshot.validate()?;
    let _lock = export_lock(dir)?;
    ensure!(
        fs::canonicalize(base)? != fs::canonicalize(dir)?,
        "Shortcut import requires a separate output directory"
    );
    write_json(dir, SNAPSHOT_FILE, &snapshot, MAX_SNAPSHOT_BYTES)?;
    write_json(dir, SYNC_FILE, &SyncStatus::default(), 4096)?;
    Ok(snapshot)
}

pub fn sync(
    mirror: &Path,
    dir: &Path,
    remote: &str,
    url: &str,
    check_interval_seconds: Option<NonZeroU32>,
) -> Result<SyncReport> {
    let project = repo_url(url)?;
    let gitlab_host = project.host_str().expect("validated GitLab project URL");
    let _lock = export_lock(dir)?;
    let previous = load_sync(dir);
    let mut stages = load_stages(dir)
        .ok()
        .filter(|stages| stages.managed)
        .unwrap_or_default();
    let previous_interval = if previous.as_ref().is_ok_and(|status| !status.managed) {
        None
    } else {
        stages.check_interval_seconds.or_else(|| {
            previous
                .as_ref()
                .ok()
                .and_then(|status| status.check_interval_seconds)
        })
    };
    let checked_interval = check_interval_seconds.or(previous_interval);
    let mut status = previous.unwrap_or_default();
    status.managed = true;
    stages.managed = true;
    stages.operation_id = operation_id();
    stages.check_interval_seconds = checked_interval;
    let result = (|| {
        let (commit, wikis) = observe(dir, &mut stages, true, || {
            let targets = wiki_targets(dir)?;
            let commit = fetch(mirror, remote, "refs/heads/main")?;
            let wikis = fetch_wikis(mirror, gitlab_host, targets)?;
            Ok(((commit.clone(), wikis), Some(commit), None))
        })?;
        status.checked_at_unix = Some(now_unix());
        let revision = observe(dir, &mut stages, false, || {
            let current = Snapshot::load(dir).ok();
            if wikis.is_empty()
                && current.as_ref().is_some_and(|snapshot| {
                    snapshot.wikis.is_empty()
                        && snapshot.shortcut_docs.is_empty()
                        && snapshot.commit == commit
                        && snapshot.repo_url == url
                })
            {
                return Ok((commit.clone(), Some(commit), Some("unchanged")));
            }
            let mut snapshot = collect(mirror, &commit, url, &[])?;
            let mut indexed_paths = HashSet::new();
            let mut skipped = 0;
            for (target, repo, commit) in wikis {
                let pages = collect(&repo, &commit, url, &target.paths)
                    .with_context(|| format!("could not export GitLab wiki {}", target.owner))?;
                let mut wiki = WikiSnapshot {
                    scope: target.scope,
                    owner: target.owner,
                    commit,
                    files: Vec::new(),
                };
                for page in pages.files {
                    let path = wiki.path(&page.path);
                    if is_markdown_path(&path) && indexed_paths.insert(path) {
                        wiki.files.push(page);
                    } else {
                        skipped += 1;
                    }
                }
                snapshot.wikis.push(wiki);
            }
            if skipped > 0 {
                eprintln!(
                    "Skipped {skipped} wiki pages with unsupported or duplicate indexed paths"
                );
            }
            snapshot.validate()?;
            let revision = snapshot.revision();
            let unchanged = current
                .as_ref()
                .is_some_and(|s| s.revision() == revision && s.repo_url == url);
            if !unchanged {
                write_json(dir, SNAPSHOT_FILE, &snapshot, MAX_SNAPSHOT_BYTES)?;
            }
            Ok((
                revision.clone(),
                Some(revision),
                unchanged.then_some("unchanged"),
            ))
        })?;
        status.upstream_commit = Some(revision);
        Ok::<_, anyhow::Error>(())
    })();
    status.last_error = result.as_ref().err().map(|error| {
        error
            .downcast_ref::<GitFailure>()
            .map_or("sync_failed", |failure| failure.code)
            .to_owned()
    });
    write_json(dir, SYNC_FILE, &status, 4096)?;
    result?;
    Ok(SyncReport {
        status,
        check_interval_seconds: checked_interval,
    })
}

fn fetch_wikis(
    mirror: &Path,
    host: &str,
    targets: Vec<WikiTarget>,
) -> Result<Vec<(WikiTarget, PathBuf, String)>> {
    let mut fetched = Vec::with_capacity(targets.len());
    for target in targets {
        let remote = format!("git@{host}:{}.wiki.git", target.owner);
        let repo = mirror.join("know-things-wiki-mirrors").join(format!(
            "{:x}",
            sha2::Sha256::digest(format!("{host}/{}/{}", target.scope.name(), target.owner))
        ));
        let commit = fetch(&repo, &remote, "HEAD")
            .with_context(|| format!("could not fetch GitLab wiki {}", target.owner))?;
        fetched.push((target, repo, commit));
    }
    Ok(fetched)
}

fn fetch(mirror: &Path, remote: &str, reference: &str) -> Result<String> {
    if !mirror.join("HEAD").is_file() {
        fs::create_dir_all(mirror)?;
        ensure!(
            fs::read_dir(mirror)?.next().is_none(),
            "sync requires a dedicated bare Git directory"
        );
        git_stdout(
            git(mirror).args(["init", "--bare"]),
            "initialize sync repository",
        )?;
    }
    ensure!(
        git_stdout(
            git(mirror).args(["rev-parse", "--is-bare-repository"]),
            "check sync repository"
        )? == b"true\n",
        "sync requires a dedicated bare Git directory"
    );
    git_stdout(
        git(mirror)
            .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes -oConnectTimeout=10 -oServerAliveInterval=10 -oServerAliveCountMax=2")
            .args(["fetch", "--no-tags", "--no-recurse-submodules", "--", remote, &format!("+{reference}:refs/know-things/main")]),
        "fetch source reference",
    )?;
    let commit = String::from_utf8(git_stdout(
        git(mirror).args(["rev-parse", "--verify", "refs/know-things/main"]),
        "resolve upstream commit",
    )?)?
    .trim()
    .to_owned();
    ensure!(is_oid(&commit), "Git returned an invalid upstream commit");
    Ok(commit)
}

fn record_stage(dir: &Path, stages: &mut SourceStages, fetch: bool, attempt: Attempt) {
    if fetch {
        &mut stages.fetch
    } else {
        &mut stages.export
    }
    .record(attempt);
    if write_json(dir, STAGES_FILE, stages, MAX_STAGE_BYTES).is_err() {
        eprintln!("KB could not publish source-stage observations");
    }
}

fn observe<T>(
    dir: &Path,
    stages: &mut SourceStages,
    fetch: bool,
    work: impl FnOnce() -> Result<(T, Option<String>, Option<&'static str>)>,
) -> Result<T> {
    let start = Instant::now();
    let mut attempt = Attempt::running(None);
    record_stage(dir, stages, fetch, attempt.clone());
    let result = work();
    match &result {
        Ok((_, commit, code)) => {
            attempt.commit.clone_from(commit);
            attempt.finish(start, Outcome::Succeeded, *code);
        }
        Err(_) => {
            let code = if fetch {
                "fetch_failed"
            } else {
                "export_failed"
            };
            attempt.finish(start, Outcome::Failed, Some(code));
        }
    }
    record_stage(dir, stages, fetch, attempt);
    result.map(|(value, _, _)| value)
}

pub fn load_stages(dir: &Path) -> Result<SourceStages> {
    read_json(&dir.join(STAGES_FILE), MAX_STAGE_BYTES)
}

pub fn load_sync(dir: &Path) -> Result<SyncStatus> {
    read_json(&dir.join(SYNC_FILE), 4096)
}

fn write_json(dir: &Path, name: &str, value: &impl Serialize, max: u64) -> Result<()> {
    local_file::write_json(&dir.join(name), value, max, 0o644)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
