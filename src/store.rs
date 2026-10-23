use crate::{
    contracts::*,
    embedding::{self, Embedder, SharedEmbedder},
    hash, identity, parser,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

type StoredContext = (String, String, String, String, i64, i64, Vec<u8>, bool);

pub struct Store {
    connection: Connection,
    _lock: File,
    embedder: Option<Arc<SharedEmbedder>>,
    directory: PathBuf,
}
#[derive(Debug)]
pub struct Staged {
    processing: String,
    job: String,
    document: String,
    desired: i64,
    revision: String,
    hash: String,
    raw: String,
    chunks: Vec<parser::Chunk>,
    vectors: Vec<Arc<Vec<u8>>>,
    space: Option<String>,
}
impl Staged {
    pub fn document_id(&self) -> &str {
        &self.document
    }
    pub(crate) fn source_hash(&self) -> &str {
        &self.hash
    }
}
pub(crate) struct Preparation {
    staged: Staged,
    markdown: bool,
    embedder: Option<Arc<SharedEmbedder>>,
}
impl Preparation {
    /// No SQLite ownership or coordinator lock is needed during this work.
    pub(crate) fn finish(self) -> Result<Staged> {
        self.finish_checked(|_| Ok(()), |_, _, _| Ok(None))
    }
    pub(crate) fn finish_checked(
        mut self,
        mut check: impl FnMut(&Staged) -> Result<()>,
        mut cached: impl FnMut(&str, &str, usize) -> Result<Option<Arc<Vec<u8>>>>,
    ) -> Result<Staged> {
        check(&self.staged)?;
        self.staged.chunks = parser::parse(&self.staged.raw, self.markdown)?;
        if let Some(embedder) = self.embedder {
            let mut local = BTreeMap::new();
            let space = embedder.manifest().space_id();
            let dimensions = embedder.manifest().dimensions;
            for c in &self.staged.chunks {
                check(&self.staged)?;
                let text = &self.staged.raw[c.start..c.end];
                let text_hash = hash(text.as_bytes());
                let vector = if let Some(vector) = local.get(&text_hash) {
                    Arc::clone(vector)
                } else {
                    let vector = if let Some(vector) = cached(&space, &text_hash, dimensions)? {
                        vector
                    } else {
                        let vector = embedder.embed(
                            text,
                            false,
                            Instant::now() + Duration::from_secs(10),
                        )?;
                        Arc::new(embedding::encode(&vector, dimensions)?)
                    };
                    local.insert(text_hash, Arc::clone(&vector));
                    vector
                };
                self.staged.vectors.push(vector);
            }
        }
        Ok(self.staged)
    }
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestReport {
    pub active: usize,
    pub unchanged: usize,
    pub excluded: usize,
    pub superseded: usize,
    pub filtered_entries: usize,
    pub failures: Vec<Value>,
}

impl Store {
    pub fn open(directory: &Path, embedder: Option<Box<dyn Embedder>>) -> Result<Self> {
        if directory.exists() && fs::symlink_metadata(directory)?.file_type().is_symlink() {
            return Err(Error::new(
                "invalid_request",
                "Store directory must not be a symlink",
            ));
        }
        fs::create_dir_all(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("owner.lock"))?;
        lock.try_lock()
            .map_err(|_| Error::new("busy", "Another coordinator owns this store"))?;
        let db = directory.join("store.sqlite");
        // Precreate without following symlinks, retaining restrictive permissions under any umask.
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&db)?;
        for suffix in ["store.sqlite-wal", "store.sqlite-shm"] {
            let path = directory.join(suffix);
            if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(Error::new(
                    "invalid_request",
                    "Store sidecars must not be symlinks",
                ));
            }
        }
        let mut connection = Connection::open(db)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > 6 {
            return Err(Error::new(
                "store_incompatible",
                "Store requires a newer Know-Things binary",
            ));
        }
        connection.pragma_update(None, "synchronous", "FULL")?;
        if version == 0 {
            connection.execute_batch(include_str!("../migrations/001.sql"))?;
        }
        if version < 2 {
            connection.execute_batch(include_str!("../migrations/002.sql"))?;
        }
        if version < 3 {
            connection.execute_batch(include_str!("../migrations/003.sql"))?;
        }
        if version < 4 {
            connection.execute_batch(include_str!("../migrations/004.sql"))?;
        }
        if version < 5 {
            crate::vector_store::migrate(&mut connection)?;
        }
        if version < 6 {
            connection.execute_batch(include_str!("../migrations/006.sql"))?;
        }
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        // Staging never changes visibility. Interrupted jobs are reconciled from sources.
        connection.execute(
            "UPDATE jobs SET status='interrupted' WHERE status IN ('processing','staged')",
            [],
        )?;
        if let Some(ref e) = embedder {
            let manifest = e.manifest();
            if manifest.dimensions == 0 || manifest.dimensions > 65536 {
                return Err(Error::new("model_incompatible", "Invalid model dimensions"));
            }
            connection.execute(
                "INSERT OR IGNORE INTO models VALUES (?1,?2)",
                params![manifest.space_id(), serde_json::to_string(manifest)?],
            )?;
        }
        Ok(Self {
            connection,
            _lock: lock,
            embedder: embedder.map(|e| Arc::new(SharedEmbedder::new(e))),
            directory: directory.into(),
        })
    }

    pub fn restore(source: &Path, directory: &Path) -> Result<Self> {
        if directory.exists() {
            return Err(Error::new(
                "invalid_request",
                "Restore destination must be a new directory",
            ));
        }
        let parent = directory
            .parent()
            .filter(|p| p.is_dir())
            .ok_or_else(|| Error::new("invalid_request", "Restore parent must exist"))?;
        let staging = parent.join(format!(
            ".restore-{}-{}",
            std::process::id(),
            hash(directory.as_os_str().as_encoded_bytes())
        ));
        fs::create_dir(&staging)?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
        let result = (|| -> Result<()> {
            let source =
                Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let version: i64 = source.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if !(1..=6).contains(&version) {
                return Err(Error::new(
                    "store_incompatible",
                    "Backup does not contain a supported Know-Things store",
                ));
            }
            let check: String = source.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            if check != "ok" {
                return Err(Error::new("store_corrupt", "Backup integrity check failed"));
            }
            source.backup(rusqlite::MAIN_DB, staging.join("store.sqlite"), None)?;
            {
                let mut restored = Self::open(&staging, None)?;
                restored.rebuild_lexical_until(Instant::now() + Duration::from_secs(120), false)?;
                restored.integrity()?;
            }
            File::open(&staging)?.sync_all()?;
            crate::filesystem::publish_directory(&staging, directory)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result?;
        Self::open(directory, None)
    }
    pub fn attach_embedder(&mut self, embedder: Box<dyn Embedder>) -> Result<()> {
        let manifest = embedder.manifest();
        if manifest.dimensions == 0 || manifest.dimensions > 65536 {
            return Err(Error::new("model_incompatible", "Invalid model dimensions"));
        }
        self.connection.execute(
            "INSERT OR IGNORE INTO models VALUES (?1,?2)",
            params![manifest.space_id(), serde_json::to_string(manifest)?],
        )?;
        self.embedder = Some(Arc::new(SharedEmbedder::new(embedder)));
        Ok(())
    }
    pub fn generation(&self) -> Result<u64> {
        read_generation(&self.connection)
    }
    pub fn register(&mut self, name: &str, root: &Path) -> Result<String> {
        self.register_with_policy(name, root, None)
    }
    pub fn register_with_policy(
        &mut self,
        name: &str,
        root: &Path,
        policy: Option<CollectionPolicy>,
    ) -> Result<String> {
        let policy = policy.map(CollectionPolicy::normalized).transpose()?;
        if name.trim().is_empty() || name.len() > 256 {
            return Err(Error::new(
                "invalid_request",
                "Collection name must have 1–256 bytes",
            ));
        }
        let root = root.canonicalize()?;
        if !root.is_dir() || self.directory.canonicalize()?.starts_with(&root) {
            return Err(Error::new(
                "invalid_request",
                "Source must be a directory outside the store",
            ));
        }
        let root = root
            .to_str()
            .ok_or_else(|| Error::new("invalid_request", "Source path must be UTF-8"))?;
        if let Some((id, old_root)) = self
            .connection
            .query_row(
                "SELECT id,root FROM collections WHERE name=?1",
                [name],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        {
            if root != old_root {
                return Err(Error::new(
                    "invalid_request",
                    "Collection name already identifies another root",
                ));
            }
            let old = crate::source_policy::load(&self.connection, &id)?;
            if policy.as_ref().is_some_and(|p| old != *p) {
                return Err(Error::new(
                    "invalid_request",
                    "Use set_collection_policy to change an existing policy",
                ));
            }
            return Ok(id);
        }
        let id = identity(&["collection", name, root]);
        let collision: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM collections WHERE id=?1 OR name=?2)",
            params![name, id],
            |r| r.get(0),
        )?;
        if collision {
            return Err(Error::new(
                "invalid_request",
                "Collection name conflicts with a collection ID",
            ));
        }
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO collections VALUES (?1,?2,?3)",
            params![id, name, root],
        )?;
        tx.execute(
            "INSERT INTO collection_policies VALUES (?1,?2)",
            params![id, serde_json::to_string(&policy.unwrap_or_default())?],
        )?;
        tx.commit()?;
        Ok(id)
    }
    pub fn collection_policy(&self, collection: &str) -> Result<CollectionPolicy> {
        let (id, _) = self.collection(collection)?;
        crate::source_policy::load(&self.connection, &id)
    }
    pub(crate) fn scheduled_collections(&self) -> Result<crate::periodic::Config> {
        let mut q = self
            .connection
            .prepare("SELECT c.id,c.root FROM collections c ORDER BY c.id")?;
        let mut scheduled = crate::periodic::Config::default();
        for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, root) = row?;
            let policy = crate::source_policy::load(&self.connection, &id)?;
            let interval = if policy.watch {
                Some(
                    policy
                        .reconcile_interval_ms
                        .unwrap_or(crate::watch::SAFETY_INTERVAL_MS)
                        .min(crate::watch::SAFETY_INTERVAL_MS),
                )
            } else {
                policy.reconcile_interval_ms
            };
            if let Some(interval) = interval {
                scheduled
                    .tasks
                    .push((crate::periodic::Task::Reconcile(id.clone()), interval));
            }
            if policy.watch {
                scheduled.watches.push(crate::watch::Config {
                    id: id.clone(),
                    root: root.into(),
                    excluded_paths: policy.excluded_paths,
                });
            }
            if let Some(retention) = policy.retention {
                scheduled
                    .tasks
                    .push((crate::periodic::Task::Retention(id), retention.interval_ms));
            }
        }
        Ok(scheduled)
    }
    pub(crate) fn source_allowed(&self, collection: &str, path: &str) -> Result<bool> {
        Ok(!self.collection_policy(collection)?.excludes(path))
    }
    pub fn set_collection_policy(
        &mut self,
        collection: &str,
        policy: CollectionPolicy,
    ) -> Result<Value> {
        let policy = policy.normalized()?;
        let (id, _) = self.collection(collection)?;
        let tx = self.connection.transaction()?;
        let old = crate::source_policy::load(&tx, &id)?;
        if old == policy {
            return Ok(json!({"policy":policy,"changed":false,"requiresReconciliation":false}));
        }
        let mut q = tx.prepare("SELECT id,path,excluded FROM documents WHERE collection_id=?1")?;
        let documents = q
            .query_map([&id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(q);
        let mut affected = 0;
        for (document, path, explicit) in documents {
            if old.excludes(&path) == policy.excludes(&path) {
                continue;
            }
            let status = if policy.excludes(&path) {
                "filtered"
            } else if explicit {
                "excluded"
            } else {
                "discovered"
            };
            let changed = tx.execute("UPDATE documents SET eligible=0,desired=desired+1,status=?2 WHERE id=?1 AND typeof(desired)='integer' AND desired>=0 AND desired<9223372036854775807", params![document,status])?;
            if changed == 0 {
                return Err(desired_failure(&tx, &document));
            }
            tx.execute("DELETE FROM lexical WHERE rowid IN (SELECT c.row_id FROM chunks c JOIN revisions r ON r.id=c.revision_id WHERE r.document_id=?1)", [&document])?;
            tx.execute("UPDATE jobs SET status='superseded' WHERE document_id=?1 AND status IN ('processing','staged')", [&document])?;
            affected += 1;
        }
        tx.execute(
            "UPDATE collection_policies SET policy=?2 WHERE collection_id=?1",
            params![id, serde_json::to_string(&policy)?],
        )?;
        increment_generation(&tx)?;
        tx.commit()?;
        Ok(
            json!({"policy":policy,"changed":true,"affectedDocuments":affected,"requiresReconciliation":true}),
        )
    }
    pub(crate) fn collection(&self, selector: &str) -> Result<(String, PathBuf)> {
        let mut query = self
            .connection
            .prepare("SELECT id,root FROM collections WHERE id=?1 OR name=?1 LIMIT 2")?;
        let mut rows = query.query([selector])?;
        let row = rows
            .next()?
            .ok_or_else(|| Error::new("invalid_request", "Unknown collection"))?;
        let result = (row.get(0)?, PathBuf::from(row.get::<_, String>(1)?));
        if rows.next()?.is_some() {
            return Err(Error::new(
                "invalid_request",
                "Ambiguous collection selector",
            ));
        }
        Ok(result)
    }
    /// Staging and activation are separate to make the late-job boundary explicit.
    pub fn stage(&mut self, collection: &str, path: &str, raw: &str) -> Result<Option<Staged>> {
        let Some(work) = self.prepare_stage(collection, path, raw)? else {
            return Ok(None);
        };
        let staged = work.finish_checked(
            |_| Ok(()),
            |space, text_hash, dimensions| self.cached_vector(space, text_hash, dimensions),
        )?;
        self.connection
            .execute("UPDATE jobs SET status='staged' WHERE id=?1", [&staged.job])?;
        Ok(Some(staged))
    }
    pub(crate) fn cached_vector(
        &self,
        space: &str,
        text_hash: &str,
        dimensions: usize,
    ) -> Result<Option<Arc<Vec<u8>>>> {
        crate::vector_store::cached(&self.connection, space, text_hash, dimensions)
    }
    pub(crate) fn prepare_stage(
        &mut self,
        collection: &str,
        path: &str,
        raw: &str,
    ) -> Result<Option<Preparation>> {
        let (collection, _) = self.collection(collection)?;
        if !self.source_allowed(&collection, path)? {
            return Ok(None);
        }
        if path.is_empty()
            || Path::new(path).is_absolute()
            || Path::new(path)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(Error::new(
                "invalid_request",
                "Expected a relative source path",
            ));
        }
        if raw.len() > 16 * 1024 * 1024 {
            return Err(Error::new(
                "resource_exhausted",
                "Document exceeds 16 MiB limit",
            ));
        }
        let document = identity(&["document", &collection, path]);
        let content_hash = hash(raw.as_bytes());
        let revision = identity(&["revision", &document, &content_hash]);
        let space = self.embedder.as_ref().map(|e| e.manifest().space_id());
        self.connection.execute("INSERT OR IGNORE INTO documents(id,collection_id,path,status) VALUES (?1,?2,?3,'discovered')",params![document,collection,path])?;
        let (active, excluded, status,processing): (Option<String>, bool, String,Option<String>) =
            self.connection.query_row(
                "SELECT active_revision,excluded,status,active_processing FROM documents WHERE id=?1",
                [&document],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?,r.get(3)?)),
            )?;
        if excluded {
            return Ok(None);
        }
        if active.as_deref() == Some(&revision)
            && status == "active"
            && processing.as_deref() == Some(parser::PROCESSING)
        {
            let missing: i64 = if let Some(ref s) = space {
                self.connection.query_row("SELECT count(*) FROM chunks c WHERE c.revision_id=?1 AND c.processing=?3 AND NOT EXISTS (SELECT 1 FROM vectors v WHERE v.chunk_id=c.id AND v.space=?2)",params![revision,s,parser::PROCESSING],|r|r.get(0))?
            } else {
                0
            };
            if missing == 0 {
                return Ok(None);
            }
        }
        let desired: i64=self.connection.query_row("UPDATE documents SET desired=desired+1,status='processing' WHERE id=?1 AND typeof(desired)='integer' AND desired>=0 AND desired<9223372036854775807 RETURNING desired",[&document],|r|r.get(0)).optional()?.ok_or_else(|| desired_failure(&self.connection, &document))?;
        let job = identity(&[
            "job",
            &document,
            &desired.to_string(),
            &content_hash,
            parser::PROCESSING,
        ]);
        self.connection.execute(
            "INSERT INTO jobs VALUES (?1,?2,?3,?4,?5,'processing',NULL)",
            params![job, document, desired, content_hash, parser::PROCESSING],
        )?;
        let markdown = Path::new(path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"));
        Ok(Some(Preparation {
            staged: Staged {
                processing: parser::PROCESSING.into(),
                job,
                document,
                desired,
                revision,
                hash: content_hash,
                raw: raw.into(),
                chunks: Vec::new(),
                vectors: Vec::new(),
                space,
            },
            markdown,
            embedder: self.embedder.clone(),
        }))
    }
    pub(crate) fn staging_current(&self, staged: &Staged) -> Result<()> {
        let current: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM documents d JOIN jobs j ON j.document_id=d.id WHERE d.id=?1 AND d.desired=?2 AND d.excluded=0 AND j.id=?3 AND j.status IN ('processing','staged'))",
            params![staged.document, staged.desired, staged.job], |r| r.get(0))?;
        if current {
            Ok(())
        } else {
            self.connection.execute("UPDATE jobs SET status='superseded' WHERE id=?1 AND status IN ('processing','staged')", [&staged.job])?;
            Err(Error::new(
                "superseded",
                "Ingestion job no longer matches the desired source state",
            ))
        }
    }
    pub fn activate(&mut self, staged: Staged) -> Result<bool> {
        let tx = self.connection.transaction()?;
        let current: Option<(i64, bool,String,String)> = tx.query_row(
            "SELECT d.desired,d.excluded,d.collection_id,d.path FROM documents d JOIN jobs j ON j.document_id=d.id WHERE d.id=?1 AND j.id=?2 AND j.status IN ('processing','staged')",
            params![staged.document, staged.job],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).optional()?;
        let allowed = if let Some((_, _, collection, path)) = &current {
            !crate::source_policy::load(&tx, collection)?.excludes(path)
        } else {
            false
        };
        if !allowed
            || current.is_none_or(|(desired, excluded, _, _)| desired != staged.desired || excluded)
        {
            tx.execute(
                "UPDATE jobs SET status='superseded' WHERE id=?1",
                [&staged.job],
            )?;
            tx.commit()?;
            return Ok(false);
        }
        tx.execute("DELETE FROM lexical WHERE rowid IN (SELECT c.row_id FROM chunks c JOIN revisions r ON r.id=c.revision_id WHERE r.document_id=?1)",[&staged.document])?;
        tx.execute(
            "INSERT OR IGNORE INTO revisions VALUES (?1,?2,?3,?4)",
            params![
                staged.revision,
                staged.document,
                staged.hash,
                staged.raw.as_bytes()
            ],
        )?;
        let path: String = tx.query_row(
            "SELECT path FROM documents WHERE id=?1",
            [&staged.document],
            |r| r.get(0),
        )?;
        for (i, c) in staged.chunks.iter().enumerate() {
            let id = identity(&[
                "chunk",
                &staged.revision,
                &staged.processing,
                &c.start.to_string(),
                &c.end.to_string(),
            ]);
            let text = &staged.raw[c.start..c.end];
            tx.execute("INSERT OR IGNORE INTO chunks(id,revision_id,start,end,heading,continuation,text,processing) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",params![id,staged.revision,c.start as i64,c.end as i64,c.heading,c.continuation,text,staged.processing])?;
            let row: i64 =
                tx.query_row("SELECT row_id FROM chunks WHERE id=?1", [&id], |r| r.get(0))?;
            tx.execute(
                "INSERT INTO lexical(rowid,text,title,heading,identifiers) VALUES (?1,?2,?3,?4,?5)",
                params![row, text, path, c.heading, identifiers(text)],
            )?;
            if let Some(ref space) = staged.space {
                crate::vector_store::put(
                    &tx,
                    &id,
                    space,
                    &hash(text.as_bytes()),
                    &staged.vectors[i],
                )?;
            }
        }
        tx.execute(
            "UPDATE documents SET active_revision=?2,active_processing=?3,eligible=1,status='active' WHERE id=?1",
            params![staged.document, staged.revision,staged.processing],
        )?;
        tx.execute("UPDATE jobs SET status='active' WHERE id=?1", [staged.job])?;
        if let Some(ref space) = staged.space {
            let missing:i64=tx.query_row("SELECT count(*) FROM chunks c JOIN documents d ON d.active_revision=c.revision_id AND d.active_processing=c.processing WHERE d.eligible=1 AND NOT EXISTS (SELECT 1 FROM vectors v WHERE v.chunk_id=c.id AND v.space=?1)",[space],|r|r.get(0))?;
            if missing == 0 {
                tx.execute("INSERT INTO meta(key,value) VALUES ('active_space',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[space])?;
            }
        }
        increment_generation(&tx)?;
        tx.commit()?;
        Ok(true)
    }
    pub fn exclude(&mut self, document: &str) -> Result<()> {
        self.hide(document, true)
    }
    fn hide(&mut self, document: &str, explicit: bool) -> Result<()> {
        let tx = self.connection.transaction()?;
        let changed=tx.execute("UPDATE documents SET eligible=0,excluded=max(excluded,?2),desired=desired+1,status='excluded' WHERE id=?1 AND typeof(desired)='integer' AND desired>=0 AND desired<9223372036854775807",params![document,explicit])?;
        if changed == 0 {
            return Err(desired_failure(&tx, document));
        }
        tx.execute("DELETE FROM lexical WHERE rowid IN (SELECT c.row_id FROM chunks c JOIN revisions r ON r.id=c.revision_id WHERE r.document_id=?1)",[document])?;
        increment_generation(&tx)?;
        tx.commit()?;
        Ok(())
    }
    pub fn reconcile(&mut self, collection: &str) -> Result<IngestReport> {
        crate::ingestion::reconcile(self, collection)
    }
    pub(crate) fn ingest_failed(&mut self, id: &str, relative: &str, code: &str) -> Result<()> {
        // Failed replacement must not leave old source content eligible.
        let document = identity(&["document", id, relative]);
        let exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM documents WHERE id=?1)",
            [&document],
            |r| r.get(0),
        )?;
        if exists {
            self.hide(&document, false)?;
            self.connection.execute(
                "UPDATE documents SET status='failed' WHERE id=?1",
                [&document],
            )?;
        }
        self.connection.execute("UPDATE jobs SET status='failed',error=?2 WHERE document_id=?1 AND status IN ('processing','staged')",params![document,code])?;
        Ok(())
    }
    pub(crate) fn ingest_missing(&mut self, id: &str, seen: &BTreeSet<String>) -> Result<usize> {
        let mut count = 0;
        let mut q = self
            .connection
            .prepare("SELECT id,path FROM documents WHERE collection_id=?1 AND eligible=1")?;
        let missing = q
            .query_map([&id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(q);
        for (document, path) in missing {
            if !seen.contains(&path) {
                self.hide(&document, false)?;
                count += 1;
            }
        }
        Ok(count)
    }
    pub fn search(&mut self, request: Search) -> Result<EvidencePack> {
        let started = Instant::now();
        if request.query.trim().is_empty()
            || request.query.len() > 8192
            || request.collections.is_empty()
            || request.collections.len() > 64
            || request.top_k == 0
            || request.top_k > 100
            || request.evidence_token_budget == 0
            || request.evidence_token_budget > 65536
            || request.deadline_ms == 0
            || request.deadline_ms > 120000
        {
            return Err(Error::new(
                "invalid_request",
                "Query, explicit scope, result limit, budget, or deadline is invalid",
            ));
        }
        let mut scope = Vec::new();
        for c in &request.collections {
            scope.push(self.collection(c)?.0);
        }
        scope.sort();
        scope.dedup();
        let deadline = started + Duration::from_millis(request.deadline_ms);
        let mut timings = BTreeMap::new();
        let embedding_start = Instant::now();
        let space = self.embedder.as_ref().map(|e| e.manifest().space_id());
        let query_vector = if request.mode == Mode::Hybrid {
            if let Some(ref mut e) = self.embedder {
                match e
                    .embed(&request.query, true, deadline)
                    .and_then(|v| embedding::encode(&v, e.manifest().dimensions))
                {
                    Ok(vector) => Some(vector),
                    Err(error) if request.strict_hybrid => return Err(error),
                    Err(_) => None,
                }
            } else {
                None
            }
        } else {
            None
        };
        timings.insert(
            "queryEmbedding".into(),
            embedding_start.elapsed().as_secs_f64() * 1000.0,
        );
        check_deadline(deadline)?;
        let tx = self.connection.transaction()?;
        tx.progress_handler(1000, Some(move || Instant::now() >= deadline))?;
        let progress_guard = ProgressGuard(&tx);
        let generation = read_generation(&tx)?;
        let scope = serde_json::to_string(&scope)?;
        let coverage: i64 = if let Some(ref space) = space {
            tx.query_row("SELECT count(*) FROM chunks c JOIN documents d ON d.active_revision=c.revision_id AND d.active_processing=c.processing WHERE d.eligible=1 AND d.collection_id IN (SELECT value FROM json_each(?1)) AND NOT EXISTS(SELECT 1 FROM vectors v WHERE v.chunk_id=c.id AND v.space=?2)",params![scope,space],|r|r.get(0))?
        } else {
            0
        };
        let active_space: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key='active_space'", [], |r| {
                r.get(0)
            })
            .optional()?;
        let dense_ready = query_vector.is_some() && coverage == 0 && space == active_space;
        let mut degradation = Vec::new();
        if request.mode == Mode::Hybrid && !dense_ready {
            if request.strict_hybrid {
                return Err(Error::new(
                    "model_unavailable",
                    "Strict hybrid requires a model and complete eligible embedding coverage",
                ));
            }
            degradation.push(
                if query_vector.is_none() {
                    "embedding_model_unavailable"
                } else {
                    "embedding_coverage_incomplete_or_upgrade_pending"
                }
                .into(),
            );
        }
        let limit = (request.top_k * 8).max(100);
        let lexical_start = Instant::now();
        let mut seen_terms = BTreeSet::new();
        let terms: Vec<String> = request
            .query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|x| !x.is_empty())
            .map(str::to_lowercase)
            .filter(|term| seen_terms.insert(term.clone()))
            .map(|x| format!("\"{}\"", x.replace('"', "\"\"")))
            .collect();
        let fts = terms.join(" OR ");
        let mut ranks = BTreeMap::<i64, f64>::new();
        if !fts.is_empty() {
            let mut stmt=tx.prepare("SELECT c.row_id FROM lexical JOIN chunks c ON c.row_id=lexical.rowid JOIN documents d ON d.active_revision=c.revision_id AND d.active_processing=c.processing WHERE lexical MATCH ?1 AND d.eligible=1 AND d.collection_id IN (SELECT value FROM json_each(?2)) ORDER BY bm25(lexical,1.0,4.0,3.0,2.0),c.id LIMIT ?3")?;
            for (rank, row) in stmt
                .query_map(params![fts, scope, limit as i64], |r| r.get::<_, i64>(0))?
                .enumerate()
            {
                ranks.insert(row?, 1.0 / (60.0 + rank as f64 + 1.0));
            }
        }
        timings.insert(
            "lexical".into(),
            lexical_start.elapsed().as_secs_f64() * 1000.0,
        );
        check_deadline(deadline)?;
        let dense_start = Instant::now();
        if dense_ready {
            let vector = query_vector.as_ref().unwrap();
            let mut stmt=tx.prepare("SELECT c.row_id,e.data FROM vectors v JOIN vector_values e ON e.id=v.value_id JOIN chunks c ON c.id=v.chunk_id JOIN documents d ON d.active_revision=c.revision_id AND d.active_processing=c.processing WHERE v.space=?1 AND d.eligible=1 AND d.collection_id IN (SELECT value FROM json_each(?2)) ORDER BY c.id")?;
            let mut rows = stmt.query(params![space, scope])?;
            // Only the best bounded candidates remain resident; vector blobs are one-row batches.
            let mut best: Vec<(i64, f64)> = Vec::new();
            while let Some(row) = rows.next()? {
                check_deadline(deadline)?;
                let id = row.get(0)?;
                let bytes: Vec<u8> = row.get(1)?;
                let score = embedding::dot(vector, &bytes)?;
                let pos = best.partition_point(|(other_id, other_score)| {
                    *other_score > score || (*other_score == score && *other_id < id)
                });
                if pos < limit {
                    best.insert(pos, (id, score));
                    best.truncate(limit);
                }
            }
            for (rank, (id, _)) in best.iter().enumerate() {
                *ranks.entry(*id).or_default() += 1.0 / (60.0 + rank as f64 + 1.0);
            }
        }
        timings.insert("dense".into(), dense_start.elapsed().as_secs_f64() * 1000.0);
        let assembly_start = Instant::now();
        let mut ranked: Vec<_> = ranks.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut evidence = Vec::new();
        let mut used = 0;
        let mut truncated = false;
        let mut stmt=tx.prepare("SELECT d.collection_id,d.id,r.id,c.id,r.hash,c.start,c.end,d.path,c.heading,c.text,c.continuation FROM chunks c JOIN revisions r ON r.id=c.revision_id JOIN documents d ON d.id=r.document_id WHERE c.row_id=?1")?;
        for (id, score) in ranked {
            check_deadline(deadline)?;
            if evidence.len() == request.top_k {
                break;
            }
            let mut item = stmt.query_row([id], |r| {
                Ok(Evidence {
                    reference: Reference {
                        collection_id: r.get(0)?,
                        document_id: r.get(1)?,
                        revision_id: r.get(2)?,
                        chunk_id: r.get(3)?,
                        content_hash: r.get(4)?,
                        byte_start: r.get::<_, i64>(5)? as usize,
                        byte_end: r.get::<_, i64>(6)? as usize,
                    },
                    source: r.get(7)?,
                    heading_path: r.get(8)?,
                    text: r.get(9)?,
                    token_count: 0,
                    full_chunk: true,
                    continuation: r.get(10)?,
                    fusion_score: score,
                })
            })?;
            let remaining = request.evidence_token_budget - used;
            let count = parser::token_count(&item.text);
            if remaining == 0 {
                truncated = true;
                break;
            }
            if count > remaining {
                let mut end = remaining.min(item.text.len());
                while !item.text.is_char_boundary(end) {
                    end -= 1;
                }
                if end == 0 {
                    truncated = true;
                    continue;
                }
                item.text.truncate(end);
                item.reference.byte_end = item.reference.byte_start + end;
                item.full_chunk = false;
                truncated = true;
            }
            item.token_count = parser::token_count(&item.text);
            used += item.token_count;
            evidence.push(item);
        }
        drop(stmt);
        drop(progress_guard);
        tx.commit()?;
        timings.insert(
            "assembly".into(),
            assembly_start.elapsed().as_secs_f64() * 1000.0,
        );
        timings.insert("total".into(), started.elapsed().as_secs_f64() * 1000.0);
        let actual = if request.mode == Mode::Hybrid && dense_ready {
            Mode::Hybrid
        } else {
            Mode::Lexical
        };
        // Timings and scores are diagnostics, excluded from pack identity.
        let canonical = json!({"version":1,"generation":generation,"scope":scope,"budget":request.evidence_token_budget,"tokenizer":parser::BUDGET_TOKENIZER,"space":space,"mode":actual,"references":evidence.iter().map(|e|(&e.reference,&e.text)).collect::<Vec<_>>()});
        let evidence_pack_id = hash(&serde_json::to_vec(&canonical)?);
        Ok(EvidencePack {
            evidence_pack_id,
            corpus_generation: generation,
            actual_mode: actual,
            embedding_space_id: if dense_ready { space } else { None },
            degradation_reasons: degradation,
            budget_tokenizer: parser::BUDGET_TOKENIZER.into(),
            token_count: used,
            truncated,
            evidence,
            timings_ms: timings,
        })
    }
    pub fn context(&self, reference: &Reference) -> Result<String> {
        let result:Option<StoredContext>=self.connection.query_row("SELECT d.collection_id,d.id,r.id,r.hash,c.start,c.end,r.raw,d.eligible FROM chunks c JOIN revisions r ON r.id=c.revision_id JOIN documents d ON d.id=r.document_id WHERE c.id=?1",[&reference.chunk_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional()?;
        let Some((collection, document, revision, content_hash, start, end, raw, eligible)) =
            result
        else {
            return Err(Error::new(
                "evidence_version_unavailable",
                "Evidence revision is unavailable",
            ));
        };
        if !eligible {
            return Err(Error::new("source_unavailable", "Source is excluded"));
        }
        if reference.collection_id != collection
            || reference.document_id != document
            || reference.revision_id != revision
            || reference.content_hash != content_hash
            || reference.byte_start < (start as usize)
            || reference.byte_end > (end as usize)
            || reference.byte_start >= reference.byte_end
        {
            return Err(Error::new(
                "invalid_request",
                "Evidence identity or span does not match",
            ));
        }
        if hash(&raw) != content_hash {
            return Err(Error::new(
                "store_corrupt",
                "Source revision failed its hash check",
            ));
        }
        let text = std::str::from_utf8(&raw)
            .map_err(|_| Error::new("store_corrupt", "Revision is not UTF-8"))?;
        text.get(reference.byte_start..reference.byte_end)
            .map(String::from)
            .ok_or_else(|| {
                Error::new(
                    "invalid_request",
                    "Span does not align with UTF-8 boundaries",
                )
            })
    }

    pub fn expand_context(
        &self,
        reference: &Reference,
        scope: ContextScope,
        budget: usize,
    ) -> Result<Value> {
        if budget == 0 || budget > 65536 {
            return Err(Error::new(
                "invalid_request",
                "Context budget must be 1–65536",
            ));
        }
        // This validates complete identity and current eligibility before expansion.
        let exact = self.context(reference)?;
        let mut candidates = Vec::new();
        if matches!(scope, ContextScope::Exact) {
            let (start, end): (i64, i64) = self.connection.query_row(
                "SELECT start,end FROM chunks WHERE id=?1",
                [&reference.chunk_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            candidates.push((
                reference.clone(),
                exact,
                reference.byte_start == start as usize && reference.byte_end == end as usize,
            ));
        } else {
            let mut stmt = self.connection.prepare(
                "SELECT id,start,end,heading,text,continuation FROM chunks WHERE revision_id=?1 AND processing=(SELECT processing FROM chunks WHERE id=?2) ORDER BY start",
            )?;
            let rows = stmt
                .query_map([&reference.revision_id, &reference.chunk_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)? as usize,
                        r.get::<_, i64>(2)? as usize,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, bool>(5)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let target = rows
                .iter()
                .position(|r| r.0 == reference.chunk_id)
                .ok_or_else(|| Error::new("evidence_version_unavailable", "Chunk unavailable"))?;

            let level_at = |i: usize| {
                if rows[i].5 {
                    None
                } else {
                    parser::heading_level(&rows[i].4)
                }
            };
            let anchor = (0..=target).rev().find(|i| level_at(*i).is_some());
            let section_start = anchor.unwrap_or(0);
            let section_end = (target + 1..rows.len())
                .find(|i| {
                    level_at(*i).is_some_and(|level| {
                        anchor.is_none() || level <= level_at(section_start).unwrap_or(0)
                    })
                })
                .unwrap_or(rows.len());
            for (i, (id, start, end, _path, text, _continuation)) in rows.into_iter().enumerate() {
                let selected = match scope {
                    ContextScope::Exact => false,
                    ContextScope::Document => true,
                    ContextScope::Neighbors => i.abs_diff(target) <= 1,
                    ContextScope::Section => i >= section_start && i < section_end,
                };
                if selected {
                    let mut selected = reference.clone();
                    selected.chunk_id = id;
                    selected.byte_start = start;
                    selected.byte_end = end;
                    candidates.push((selected, text, true));
                }
            }
            // Always spend the budget on the requested chunk before neighboring context.
            candidates.sort_by_key(|(r, _, _)| (r.chunk_id != reference.chunk_id, r.byte_start));
        }
        let mut selected = Vec::new();
        let mut used = 0;
        let mut truncated = false;
        for (mut reference, mut text, whole_chunk) in candidates {
            let remaining = budget - used;
            if remaining == 0 {
                truncated = true;
                break;
            }
            let full = text.len() <= remaining;
            if !full {
                let mut end = remaining;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                reference.byte_end = reference.byte_start + end;
                truncated = true;
            }
            if text.is_empty() {
                continue;
            }
            used += text.len();
            selected
                .push(json!({"reference":reference,"text":text,"fullChunk":full && whole_chunk}));
        }
        selected.sort_by_key(|e| e["reference"]["byteStart"].as_u64());
        let text = selected
            .iter()
            .filter_map(|e| e["text"].as_str())
            .collect::<Vec<_>>()
            .join("");
        Ok(
            json!({"reference":reference,"text":text,"evidence":selected,"scope":scope,"tokenCount":used,"budgetTokenizer":parser::BUDGET_TOKENIZER,"truncated":truncated}),
        )
    }
    pub fn include(&mut self, document: &str) -> Result<Value> {
        let scope: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT collection_id,path FROM documents WHERE id=?1",
                [document],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((collection, path)) = scope
            && !self.source_allowed(&collection, &path)?
        {
            return Err(Error::new(
                "source_unavailable",
                "Document is excluded by collection policy",
            ));
        }
        let tx = self.connection.transaction()?;
        let changed=tx.execute("UPDATE documents SET excluded=0,eligible=0,desired=desired+1,status='discovered' WHERE id=?1 AND typeof(desired)='integer' AND desired>=0 AND desired<9223372036854775807",[document])?;
        if changed == 0 {
            return Err(desired_failure(&tx, document));
        }
        tx.execute("DELETE FROM lexical WHERE rowid IN (SELECT c.row_id FROM chunks c JOIN revisions r ON r.id=c.revision_id WHERE r.document_id=?1)",[document])?;
        increment_generation(&tx)?;
        tx.commit()?;
        Ok(json!({"included":true,"requiresReconciliation":true}))
    }
    pub fn remove_collection(&mut self, collection: &str) -> Result<Value> {
        let (id, _) = self.collection(collection)?;
        let tx = self.connection.transaction()?;
        tx.execute("DELETE FROM lexical WHERE rowid IN (SELECT c.row_id FROM chunks c JOIN revisions r ON r.id=c.revision_id JOIN documents d ON d.id=r.document_id WHERE d.collection_id=?1)",[&id])?;
        tx.execute("DELETE FROM vectors WHERE chunk_id IN (SELECT c.id FROM chunks c JOIN revisions r ON r.id=c.revision_id JOIN documents d ON d.id=r.document_id WHERE d.collection_id=?1)",[&id])?;
        tx.execute("DELETE FROM chunks WHERE revision_id IN (SELECT r.id FROM revisions r JOIN documents d ON d.id=r.document_id WHERE d.collection_id=?1)",[&id])?;
        tx.execute("DELETE FROM revisions WHERE document_id IN (SELECT id FROM documents WHERE collection_id=?1)",[&id])?;
        tx.execute("DELETE FROM jobs WHERE document_id IN (SELECT id FROM documents WHERE collection_id=?1)",[&id])?;
        tx.execute("DELETE FROM documents WHERE collection_id=?1", [&id])?;
        tx.execute("DELETE FROM collections WHERE id=?1", [&id])?;
        increment_generation(&tx)?;
        tx.commit()?;
        Ok(json!({"removed":true,"secureErasure":false}))
    }
    pub fn prune(&mut self, retain: usize) -> Result<Value> {
        self.prune_with_leases(retain, None, 5000, &BTreeSet::new())
    }
    pub(crate) fn prune_with_leases(
        &mut self,
        retain: usize,
        max_revisions: Option<usize>,
        deadline_ms: u64,
        leases: &BTreeSet<String>,
    ) -> Result<Value> {
        if retain == 0
            || retain > 1000
            || max_revisions.is_some_and(|n| !(1..=1000).contains(&n))
            || !(1..=120000).contains(&deadline_ms)
        {
            return Err(Error::new(
                "invalid_request",
                "Retain 1–1000 revisions; optional batch limit is 1–1000 and deadline is 1–120000 ms",
            ));
        }
        let deadline = Instant::now() + Duration::from_millis(deadline_ms);
        self.prune_until(retain, max_revisions, deadline, leases)
    }
    fn prune_until(
        &mut self,
        retain: usize,
        max_revisions: Option<usize>,
        deadline: Instant,
        leases: &BTreeSet<String>,
    ) -> Result<Value> {
        self.prune_until_scoped(retain, max_revisions, deadline, leases, None)
    }
    pub(crate) fn prune_collection_with_leases(
        &mut self,
        collection: &str,
        policy: &RetentionPolicy,
        leases: &BTreeSet<String>,
    ) -> Result<Value> {
        policy.validate()?;
        let (id, _) = self.collection(collection)?;
        self.prune_until_scoped(
            policy.retain_revisions,
            Some(policy.max_revisions),
            Instant::now() + Duration::from_millis(policy.deadline_ms),
            leases,
            Some(&id),
        )
    }
    fn prune_until_scoped(
        &mut self,
        retain: usize,
        max_revisions: Option<usize>,
        deadline: Instant,
        leases: &BTreeSet<String>,
        collection: Option<&str>,
    ) -> Result<Value> {
        check_deadline(deadline)?;
        let tx = self.connection.transaction()?;
        tx.progress_handler(1000, Some(move || Instant::now() >= deadline))?;
        let progress = ProgressGuard(&tx);
        let leases = serde_json::to_string(leases)?;
        tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS prune_revisions(id TEXT PRIMARY KEY); DELETE FROM prune_revisions;")?;
        tx.execute("INSERT INTO prune_revisions SELECT id FROM (SELECT r.id,row_number() OVER (PARTITION BY r.document_id ORDER BY (r.id=d.active_revision) DESC,r.rowid DESC) AS position FROM revisions r JOIN documents d ON d.id=r.document_id WHERE (?4 IS NULL OR d.collection_id=?4)) WHERE position>?1 AND id NOT IN (SELECT value FROM json_each(?2)) ORDER BY id LIMIT ?3", params![retain as i64, leases, max_revisions.map(|n| n as i64).unwrap_or(-1),collection])?;
        tx.execute("DELETE FROM vectors WHERE chunk_id IN (SELECT id FROM chunks WHERE revision_id IN (SELECT id FROM prune_revisions))",[])?;
        tx.execute("DELETE FROM lexical WHERE rowid IN (SELECT row_id FROM chunks WHERE revision_id IN (SELECT id FROM prune_revisions))",[])?;
        tx.execute(
            "DELETE FROM chunks WHERE revision_id IN (SELECT id FROM prune_revisions)",
            [],
        )?;
        let removed = tx.execute(
            "DELETE FROM revisions WHERE id IN (SELECT id FROM prune_revisions)",
            [],
        )?;
        let (remaining, deferred): (i64,i64) = tx.query_row("SELECT count(*),coalesce(sum(id IN (SELECT value FROM json_each(?2))),0) FROM (SELECT r.id,row_number() OVER (PARTITION BY r.document_id ORDER BY (r.id=d.active_revision) DESC,r.rowid DESC) AS position FROM revisions r JOIN documents d ON d.id=r.document_id WHERE (?3 IS NULL OR d.collection_id=?3)) WHERE position>?1", params![retain as i64, leases,collection], |r| Ok((r.get(0)?, r.get(1)?)))?;
        check_deadline(deadline)?;
        // Automatic passes do not publish an empty corpus generation.
        if removed > 0 || collection.is_none() {
            increment_generation(&tx)?;
        }
        tx.execute("DELETE FROM prune_revisions", [])?;
        drop(progress);
        tx.commit()?;
        Ok(
            json!({"revisionsRemoved":removed,"retainedPerDocument":retain,"revisionsRemaining":remaining,"leasedRevisionsDeferred":deferred,"moreRemaining":remaining>0,"secureErasure":false}),
        )
    }
    pub fn integrity(&self) -> Result<Value> {
        crate::integrity::canonical(&self.connection, None)?;
        crate::integrity::lexical(&self.connection, None)?;
        crate::integrity::database(&self.connection)?;
        Ok(
            json!({"sqlite":"ok","foreignKeyViolations":0,"ftsVisibilityMismatches":0,"canonicalSpans":"ok","vectors":"ok"}),
        )
    }
    /// Rebuild only derived lexical data; canonical revisions/vectors are verified first.
    pub fn rebuild_lexical(&mut self, deadline_ms: u64) -> Result<Value> {
        if deadline_ms == 0 || deadline_ms > 120_000 {
            return Err(Error::new(
                "invalid_request",
                "Rebuild deadline must be 1–120000 ms",
            ));
        }
        self.rebuild_lexical_until(Instant::now() + Duration::from_millis(deadline_ms), true)
    }
    fn rebuild_lexical_until(
        &mut self,
        deadline: Instant,
        advance_generation: bool,
    ) -> Result<Value> {
        let tx = self.connection.transaction()?;
        tx.progress_handler(1000, Some(move || Instant::now() >= deadline))?;
        let progress = ProgressGuard(&tx);
        crate::integrity::canonical(&tx, Some(deadline))?;
        check_deadline(deadline)?;
        tx.execute_batch("DROP TABLE IF EXISTS lexical; CREATE VIRTUAL TABLE lexical USING fts5(text,title,heading,identifiers,tokenize='unicode61');")?;
        let mut count = 0usize;
        {
            let mut statement = tx.prepare("SELECT c.row_id,c.text,d.path,c.heading FROM chunks c JOIN documents d ON d.active_revision=c.revision_id AND d.active_processing=c.processing WHERE d.eligible=1 ORDER BY c.row_id")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                check_deadline(deadline)?;
                let id: i64 = row.get(0)?;
                let text: String = row.get(1)?;
                let title: String = row.get(2)?;
                let heading: String = row.get(3)?;
                tx.execute("INSERT INTO lexical(rowid,text,title,heading,identifiers) VALUES (?1,?2,?3,?4,?5)", params![id,text,title,heading,identifiers(&text)])?;
                count += 1;
            }
        }
        crate::integrity::lexical(&tx, Some(deadline))?;
        crate::integrity::database(&tx)?;
        check_deadline(deadline)?;
        if advance_generation {
            increment_generation(&tx)?;
        }
        let generation = read_generation(&tx)?;
        drop(progress);
        tx.commit()?;
        Ok(
            json!({"rebuilt":"lexical","eligibleChunks":count,"corpusGeneration":generation,"referencesPreserved":true,"sourceFilesRead":false}),
        )
    }
    pub fn backup(&self, destination: &Path) -> Result<Value> {
        let parent = destination.parent().ok_or_else(|| {
            Error::new(
                "invalid_request",
                "Backup requires an explicit parent directory",
            )
        })?;
        if !parent.is_dir() {
            return Err(Error::new(
                "invalid_request",
                "Backup parent directory must exist",
            ));
        }
        match fs::symlink_metadata(destination) {
            Ok(_) => {
                return Err(Error::new(
                    "invalid_request",
                    "Backup destination already exists",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let staging = parent.join(format!(
            ".backup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let placeholder = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staging)?;
        drop(placeholder);
        let result = (|| -> Result<()> {
            self.connection.backup(rusqlite::MAIN_DB, &staging, None)?;
            File::open(&staging)?.sync_all()?;
            // Same-directory hard-link publication is atomic and refuses to
            // overwrite a destination created concurrently by another process.
            fs::hard_link(&staging, destination)?;
            fs::remove_file(&staging)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&staging);
        }
        result?;
        Ok(json!({"destination":destination,"corpusGeneration":self.generation()?}))
    }
    pub fn status(&self) -> Result<Value> {
        let mut stmt=self.connection.prepare("SELECT c.id,c.name,c.root,d.status,count(d.id) FROM collections c LEFT JOIN documents d ON d.collection_id=c.id GROUP BY c.id,d.status ORDER BY c.name,d.status")?;
        let rows=stmt.query_map([],|r|Ok(json!({"collectionId":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"root":r.get::<_,String>(2)?,"status":r.get::<_,Option<String>>(3)?,"documents":r.get::<_,i64>(4)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        let chunks:i64=self.connection.query_row("SELECT count(*) FROM chunks c JOIN documents d ON d.active_revision=c.revision_id AND d.active_processing=c.processing WHERE d.eligible=1",[],|r|r.get(0))?;
        let bytes:i64=self.connection.query_row("SELECT coalesce(sum(length(r.raw)),0) FROM revisions r JOIN documents d ON d.active_revision=r.id WHERE d.eligible=1",[],|r|r.get(0))?;
        let active_space: Option<String> = self
            .connection
            .query_row("SELECT value FROM meta WHERE key='active_space'", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(
            json!({"collections":rows,"eligibleChunks":chunks,"eligibleSourceBytes":bytes,"configuredEmbeddingSpaceId":self.embedder.as_ref().map(|e|e.manifest().space_id()),"activeEmbeddingSpaceId":active_space,"generationAvailable":false}),
        )
    }
    pub fn execute(&mut self, request: Request) -> Response {
        self.execute_with_leases(request, &BTreeSet::new())
    }
    pub(crate) fn execute_with_leases(
        &mut self,
        request: Request,
        leases: &BTreeSet<String>,
    ) -> Response {
        let started = Instant::now();
        let result = (|| -> Result<Value> {
            if request.schema_version != 1 {
                return Err(Error::new(
                    "unsupported_version",
                    "Expected schemaVersion 1",
                ));
            }
            if request.request_id.is_empty() || request.request_id.len() > 256 {
                return Err(Error::new(
                    "invalid_request",
                    "requestId must have 1–256 bytes",
                ));
            }
            match request.operation {
                Operation::Answer { .. } | Operation::Cancel { .. } => Err(Error::new(
                    "unsupported_capability",
                    "Answer operations require the coordinator",
                )),
                Operation::Register { name, root, policy } => Ok(
                    json!({"collectionId":self.register_with_policy(&name,Path::new(&root),policy)?}),
                ),
                Operation::CollectionPolicy { collection } => {
                    Ok(serde_json::to_value(self.collection_policy(&collection)?)?)
                }
                Operation::SetCollectionPolicy { collection, policy } => {
                    self.set_collection_policy(&collection, policy)
                }
                Operation::Reconcile { collection } => {
                    Ok(serde_json::to_value(self.reconcile(&collection)?)?)
                }
                Operation::Search { request } => Ok(serde_json::to_value(self.search(request)?)?),
                Operation::Context {
                    reference,
                    scope,
                    evidence_token_budget,
                } => self.expand_context(&reference, scope, evidence_token_budget),
                Operation::Exclude { document } => {
                    self.exclude(&document)?;
                    Ok(json!({"excluded":true,"physicallyRemoved":false}))
                }
                Operation::Include { document } => self.include(&document),
                Operation::RemoveCollection { collection } => self.remove_collection(&collection),
                Operation::Prune {
                    retain_revisions,
                    max_revisions,
                    deadline_ms,
                } => self.prune_with_leases(retain_revisions, max_revisions, deadline_ms, leases),
                Operation::Status => self.status(),
                Operation::Integrity => self.integrity(),
                Operation::RebuildLexical { deadline_ms } => self.rebuild_lexical(deadline_ms),
                Operation::Backup { destination } => self.backup(Path::new(&destination)),
            }
        })();
        let (data, error) = match result {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        Response {
            schema_version: 1,
            request_id: request.request_id,
            status: if error.is_none() { "ok" } else { "error" }.into(),
            duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            corpus_generation: self.generation().ok(),
            data,
            error,
        }
    }
}
fn desired_failure(connection: &Connection, document: &str) -> Error {
    let value = connection
        .query_row(
            "SELECT desired FROM documents WHERE id=?1",
            [document],
            |r| r.get::<_, rusqlite::types::Value>(0),
        )
        .optional();
    match value {
        Ok(Some(rusqlite::types::Value::Integer(value))) if value == i64::MAX => Error::new(
            "resource_exhausted",
            "Document lifecycle capacity is exhausted",
        ),
        Ok(None) => Error::new("source_unavailable", "Unknown document"),
        Err(error) => error.into(),
        _ => Error::new("store_corrupt", "Invalid document lifecycle counter"),
    }
}
fn read_generation(connection: &Connection) -> Result<u64> {
    let value: Option<String> = connection
        .query_row("SELECT value FROM meta WHERE key='generation'", [], |r| {
            r.get(0)
        })
        .optional()?;
    value
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|value| *value >= 0)
        .map(|value| value as u64)
        .ok_or_else(|| Error::new("store_corrupt", "Invalid canonical generation"))
}
fn increment_generation(connection: &Connection) -> Result<()> {
    let next = read_generation(connection)?
        .checked_add(1)
        .filter(|next| *next <= i64::MAX as u64)
        .ok_or_else(|| {
            Error::new(
                "resource_exhausted",
                "Corpus generation capacity is exhausted",
            )
        })?;
    connection.execute(
        "UPDATE meta SET value=?1 WHERE key='generation'",
        [next.to_string()],
    )?;
    Ok(())
}
pub(crate) fn identifiers(text: &str) -> String {
    text.split_whitespace()
        .filter(|w| w.contains(['.', '/', '-', '_']))
        .collect::<Vec<_>>()
        .join(" ")
}
fn check_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(Error::new("deadline_exceeded", "Search deadline exceeded"))
    } else {
        Ok(())
    }
}

struct ProgressGuard<'a>(&'a Connection);
impl Drop for ProgressGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

#[cfg(test)]
mod resource_tests {
    use super::*;
    #[test]
    fn sqlite_capacity_failure_rolls_back_activation_and_allows_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let directory = temp.path().join("data");
        let mut store = Store::open(&directory, None).unwrap();
        store.register("kb", &source).unwrap();
        let first = store
            .stage("kb", "guide.md", "original policy")
            .unwrap()
            .unwrap();
        store.activate(first).unwrap();
        let query = |text: &str| Search {
            query: text.into(),
            collections: vec!["kb".into()],
            top_k: 20,
            evidence_token_budget: 65536,
            deadline_ms: 5000,
            mode: Mode::Lexical,
            strict_hybrid: false,
        };
        let original = store.search(query("original")).unwrap().evidence.remove(0);
        let generation = store.generation().unwrap();
        let raw = "replacement ".repeat(100_000);
        let staged = store.stage("kb", "guide.md", &raw).unwrap().unwrap();
        let pages: i64 = store
            .connection
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        store
            .connection
            .pragma_update(None, "max_page_count", pages)
            .unwrap();
        assert_eq!(
            store.activate(staged).unwrap_err().code,
            "resource_exhausted"
        );
        assert_eq!(store.generation().unwrap(), generation);
        assert_eq!(store.context(&original.reference).unwrap(), original.text);
        assert_eq!(store.search(query("original")).unwrap().evidence.len(), 1);
        assert!(
            store
                .search(query("replacement"))
                .unwrap()
                .evidence
                .is_empty()
        );
        store.integrity().unwrap();
        // Only this disposable database's pager limit changed; no filesystem
        // was filled and no other process's resources were restricted.
        store
            .connection
            .pragma_update(None, "max_page_count", 1_000_000)
            .unwrap();
        let retry = store.stage("kb", "guide.md", &raw).unwrap().unwrap();
        assert!(store.activate(retry).unwrap());
        assert_eq!(store.generation().unwrap(), generation + 1);
        assert!(store.search(query("original")).unwrap().evidence.is_empty());
        store.integrity().unwrap();
        drop(store);
        let mut reopened = Store::open(&directory, None).unwrap();
        assert_eq!(
            reopened.context(&original.reference).unwrap(),
            original.text
        );
        assert!(
            !reopened
                .search(query("replacement"))
                .unwrap()
                .evidence
                .is_empty()
        );
        reopened.integrity().unwrap();
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn pruning_failure_or_expired_deadline_preserves_every_revision_and_generation() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let mut store = Store::open(&temp.path().join("data"), None).unwrap();
        store.register("kb", &source).unwrap();
        for text in ["old policy", "current policy"] {
            let staged = store.stage("kb", "guide.md", text).unwrap().unwrap();
            store.activate(staged).unwrap();
        }
        let generation = store.generation().unwrap();
        for (batch, deadline) in [
            (Some(0), 5000),
            (Some(1001), 5000),
            (Some(1), 0),
            (None, 120001),
        ] {
            assert_eq!(
                store
                    .prune_with_leases(1, batch, deadline, &BTreeSet::new())
                    .unwrap_err()
                    .code,
                "invalid_request"
            );
        }
        assert_eq!(
            store
                .prune_until(
                    1,
                    Some(1),
                    Instant::now() - Duration::from_secs(1),
                    &BTreeSet::new()
                )
                .unwrap_err()
                .code,
            "deadline_exceeded"
        );
        let canonical = |store: &Store| -> Vec<(String, Vec<u8>)> {
            store
                .connection
                .prepare("SELECT id,raw FROM revisions ORDER BY id")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let before = canonical(&store);
        // Fail after deletion, at the final transactional generation marker.
        store.connection.execute_batch("CREATE TEMP TRIGGER reject_prune BEFORE UPDATE OF value ON main.meta WHEN OLD.key='generation' BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
        assert!(
            store
                .prune_with_leases(1, Some(1), 5000, &BTreeSet::new())
                .is_err()
        );
        assert_eq!(canonical(&store), before);
        assert_eq!(store.generation().unwrap(), generation);
        store.integrity().unwrap();
        store
            .connection
            .execute_batch("DROP TRIGGER reject_prune")
            .unwrap();
        assert_eq!(store.prune(1).unwrap()["revisionsRemoved"], 1);
        store.integrity().unwrap();
        drop(store);
        Store::open(&temp.path().join("data"), None)
            .unwrap()
            .integrity()
            .unwrap();
    }
    #[test]
    fn failed_or_expired_rebuild_does_not_publish_partial_repair() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let mut store = Store::open(&temp.path().join("data"), None).unwrap();
        store.register("kb", &source).unwrap();
        let staged = store
            .stage("kb", "guide.md", "policy redis.example")
            .unwrap()
            .unwrap();
        store.activate(staged).unwrap();
        let generation = store.generation().unwrap();
        assert_eq!(
            store.rebuild_lexical(0).unwrap_err().code,
            "invalid_request"
        );
        assert_eq!(
            store.rebuild_lexical(120001).unwrap_err().code,
            "invalid_request"
        );
        assert_eq!(
            store
                .rebuild_lexical_until(Instant::now() - Duration::from_secs(1), true)
                .unwrap_err()
                .code,
            "deadline_exceeded"
        );
        store.integrity().unwrap();
        store
            .connection
            .execute("UPDATE lexical SET identifiers='wrong.identifier'", [])
            .unwrap();
        assert!(store.integrity().is_err());
        // Fail after rebuilding/validating FTS, at its final publication marker.
        store.connection.execute_batch("CREATE TEMP TRIGGER reject_generation BEFORE UPDATE OF value ON main.meta WHEN OLD.key='generation' BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
        assert!(store.rebuild_lexical(5000).is_err());
        assert_eq!(store.generation().unwrap(), generation);
        assert_eq!(
            store
                .connection
                .query_row("SELECT identifiers FROM lexical", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "wrong.identifier"
        );
        store
            .connection
            .execute_batch("DROP TRIGGER reject_generation")
            .unwrap();
        store.rebuild_lexical(5000).unwrap();
        store.integrity().unwrap();
        assert_eq!(store.generation().unwrap(), generation + 1);
        assert_eq!(
            store
                .connection
                .query_row("SELECT identifiers FROM lexical", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "redis.example"
        );
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    #[test]
    fn exhausted_or_invalid_generations_cannot_publish_changes_or_silent_zero_snapshots() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let mut store = Store::open(&temp.path().join("data"), None).unwrap();
        store.register("kb", &source).unwrap();
        let staged = store.stage("kb", "guide.md", "policy").unwrap().unwrap();
        store.activate(staged).unwrap();
        let query = || Search {
            query: "policy".into(),
            collections: vec!["kb".into()],
            top_k: 20,
            evidence_token_budget: 1000,
            deadline_ms: 5000,
            mode: Mode::Lexical,
            strict_hybrid: false,
        };
        let evidence = store.search(query()).unwrap().evidence.remove(0);
        store
            .connection
            .execute(
                "UPDATE meta SET value=?1 WHERE key='generation'",
                [i64::MAX.to_string()],
            )
            .unwrap();
        assert_eq!(
            store.rebuild_lexical(5000).unwrap_err().code,
            "resource_exhausted"
        );
        assert_eq!(
            store
                .exclude(&evidence.reference.document_id)
                .unwrap_err()
                .code,
            "resource_exhausted"
        );
        assert_eq!(store.generation().unwrap(), i64::MAX as u64);
        assert_eq!(store.context(&evidence.reference).unwrap(), "policy");
        assert_eq!(store.search(query()).unwrap().evidence.len(), 1);
        store.integrity().unwrap();
        store
            .connection
            .execute("UPDATE meta SET value='invalid' WHERE key='generation'", [])
            .unwrap();
        assert_eq!(store.generation().unwrap_err().code, "store_corrupt");
        assert_eq!(store.search(query()).unwrap_err().code, "store_corrupt");
    }
}

#[cfg(test)]
mod lifecycle_capacity_tests {
    use super::*;
    #[test]
    fn exhausted_document_counters_do_not_round_or_partially_change_eligibility() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let mut store = Store::open(&temp.path().join("data"), None).unwrap();
        store.register("kb", &source).unwrap();
        let staged = store.stage("kb", "guide.md", "policy").unwrap().unwrap();
        let document = staged.document.clone();
        store.activate(staged).unwrap();
        let generation = store.generation().unwrap();
        store
            .connection
            .execute(
                "UPDATE documents SET desired=?1 WHERE id=?2",
                params![i64::MAX, document],
            )
            .unwrap();
        assert_eq!(
            store
                .stage("kb", "guide.md", "replacement")
                .unwrap_err()
                .code,
            "resource_exhausted"
        );
        assert_eq!(
            store.exclude(&document).unwrap_err().code,
            "resource_exhausted"
        );
        assert_eq!(
            store.include(&document).unwrap_err().code,
            "resource_exhausted"
        );
        let state = store
            .connection
            .query_row(
                "SELECT desired,eligible,excluded,status FROM documents WHERE id=?1",
                [&document],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, bool>(1)?,
                        r.get::<_, bool>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(state, (i64::MAX, true, false, "active".into()));
        assert_eq!(store.generation().unwrap(), generation);
        store.integrity().unwrap();
    }
}
