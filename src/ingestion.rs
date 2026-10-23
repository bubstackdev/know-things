//! Shared read-only source scan. Targets own short publication transactions.
use crate::{
    contracts::*,
    hash,
    store::{IngestReport, Preparation, Staged, Store},
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
pub(crate) trait Target {
    fn check(&mut self) -> Result<()> {
        Ok(())
    }
    fn collection(&mut self, selector: &str) -> Result<(String, PathBuf)>;
    fn policy(&mut self, collection: &str) -> Result<CollectionPolicy>;
    fn allowed(&mut self, collection: &str, path: &str) -> Result<bool> {
        Ok(!self.policy(collection)?.excludes(path))
    }
    fn prepare(&mut self, collection: &str, path: &str, raw: &str) -> Result<Option<Preparation>>;
    fn publish(&mut self, staged: Staged) -> Result<bool>;
    fn finish(&mut self, preparation: Preparation) -> Result<Staged> {
        preparation.finish()
    }
    fn failed(&mut self, collection: &str, path: &str, code: &str) -> Result<()>;
    fn missing(&mut self, collection: &str, seen: &BTreeSet<String>) -> Result<usize>;
}
impl Target for Store {
    fn policy(&mut self, collection: &str) -> Result<CollectionPolicy> {
        self.collection_policy(collection)
    }
    fn collection(&mut self, selector: &str) -> Result<(String, PathBuf)> {
        Store::collection(self, selector)
    }
    fn prepare(&mut self, collection: &str, path: &str, raw: &str) -> Result<Option<Preparation>> {
        self.prepare_stage(collection, path, raw)
    }
    fn publish(&mut self, staged: Staged) -> Result<bool> {
        self.activate(staged)
    }
    fn finish(&mut self, preparation: Preparation) -> Result<Staged> {
        preparation.finish_checked(
            |_| Ok(()),
            |space, text_hash, dimensions| self.cached_vector(space, text_hash, dimensions),
        )
    }
    fn failed(&mut self, collection: &str, path: &str, code: &str) -> Result<()> {
        self.ingest_failed(collection, path, code)
    }
    fn missing(&mut self, collection: &str, seen: &BTreeSet<String>) -> Result<usize> {
        self.ingest_missing(collection, seen)
    }
}
pub(crate) fn reconcile(target: &mut impl Target, collection: &str) -> Result<IngestReport> {
    target.check()?;
    let (id, root) = target.collection(collection)?;
    let policy = target.policy(&id)?;
    if root.canonicalize()? != root || !root.is_dir() {
        return Err(Error::new(
            "source_unavailable",
            "Collection root changed or is unavailable",
        ));
    }
    let source_root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&root)?;
    let mut report = IngestReport {
        active: 0,
        unchanged: 0,
        excluded: 0,
        superseded: 0,
        filtered_entries: 0,
        failures: Vec::new(),
    };
    let mut seen = BTreeSet::new();
    let filtered = std::cell::Cell::new(0usize);
    let walker = walkdir::WalkDir::new(&root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            if e.path()
                .strip_prefix(&root)
                .ok()
                .and_then(Path::to_str)
                .is_some_and(|relative| !relative.is_empty() && policy.excludes(relative))
            {
                filtered.set(filtered.get() + 1);
                return false;
            }
            !e.file_type().is_dir()
                || !matches!(
                    e.file_name().to_str(),
                    Some(".git" | "node_modules" | "target" | "dist" | "build" | ".private")
                )
        });
    let mut traversal_failed = false;
    for entry in walker {
        target.check()?;
        let entry = match entry {
            Ok(x) => x,
            Err(_) => {
                traversal_failed = true;
                report.failures.push(json!({"code":"source_unavailable"}));
                continue;
            }
        };
        if entry.file_type().is_dir() {
            continue;
        }
        let Some(relative) = entry.path().strip_prefix(&root).ok().and_then(Path::to_str) else {
            traversal_failed = true;
            report.failures.push(json!({"code":"invalid_path"}));
            continue;
        };
        seen.insert(relative.to_owned());
        if !target.allowed(&id, relative)? {
            filtered.set(filtered.get() + 1);
            continue;
        }
        let supported = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown" | "txt"));
        let result = (|| -> Result<Option<Staged>> {
            if !supported {
                return Err(Error::new(
                    "unsupported_format",
                    "Only Markdown and UTF-8 text are supported",
                ));
            }
            if !entry.file_type().is_file() || !entry.path().canonicalize()?.starts_with(&root) {
                return Err(Error::new(
                    "source_unavailable",
                    "Symlinks and non-regular sources are excluded",
                ));
            }
            let raw = read_source(&source_root, Path::new(relative))?;
            let raw = String::from_utf8(raw)
                .map_err(|_| Error::new("parse_error", "Source is not valid UTF-8"))?;
            let staged = target
                .prepare(&id, relative, &raw)?
                .map(|p| target.finish(p))
                .transpose()?;
            // Reject edits/removals between read/embedding and publication.
            if let Some(ref staged) = staged {
                target.check()?;
                let bytes = read_source(&source_root, Path::new(relative))?;
                if hash(&bytes) != staged.source_hash() {
                    return Err(Error::new(
                        "source_changed",
                        "Source changed during ingestion; reconcile again",
                    ));
                }
            }
            Ok(staged)
        })();
        match result {
            Ok(Some(staged)) => {
                target.check()?;
                if target.publish(staged)? {
                    report.active += 1;
                } else {
                    report.superseded += 1;
                }
            }
            Ok(None) => report.unchanged += 1,
            Err(e) if e.code == "superseded" => {
                report.superseded += 1;
            }
            Err(e) if e.code == "canceled" => return Err(e),
            Err(e) => {
                target.failed(&id, relative, &e.code)?;
                report.failures.push(json!({"path":relative,"code":e.code}));
            }
        }
    }
    // Never infer deletion from an incomplete directory traversal.
    if !traversal_failed {
        target.check()?;
        report.excluded += target.missing(&id, &seen)?;
    }
    report.filtered_entries = filtered.get();
    Ok(report)
}

// Resolve every component relative to the opened collection, refusing symlinks
// and special files even if the source tree changes during an embedding call.
fn read_source(root: &File, relative: &Path) -> Result<Vec<u8>> {
    let components: Vec<_> = relative.components().collect();
    if components.is_empty() {
        return Err(Error::new(
            "invalid_path",
            "Expected a relative source path",
        ));
    }
    let mut current = root.try_clone()?;
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            return Err(Error::new(
                "invalid_path",
                "Expected a relative source path",
            ));
        };
        let name = std::ffi::CString::new(name.as_bytes())
            .map_err(|_| Error::new("invalid_path", "Invalid source path"))?;
        let is_directory = index + 1 < components.len();
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if is_directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: the directory remains owned here; the successful descriptor
        // is immediately adopted by File and closed on every exit path.
        let descriptor = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(Error::new(
                "source_unavailable",
                "Source changed or cannot be read safely",
            ));
        }
        current = unsafe { File::from_raw_fd(descriptor) };
    }
    if !current.metadata()?.is_file() {
        return Err(Error::new(
            "source_unavailable",
            "Source is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    current.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(Error::new("resource_exhausted", "Source exceeds 16 MiB"));
    }
    Ok(bytes)
}
