//! Canonical validation independent of the rebuildable lexical index.
use crate::{contracts::*, embedding, hash, identity};
use rusqlite::{Connection, OptionalExtension};
use std::{collections::BTreeMap, time::Instant};
fn check_deadline(deadline: Option<Instant>) -> Result<()> {
    if deadline.is_some_and(|at| Instant::now() >= at) {
        Err(Error::new(
            "deadline_exceeded",
            "Integrity/rebuild deadline exceeded",
        ))
    } else {
        Ok(())
    }
}
pub(crate) fn database(connection: &Connection) -> Result<()> {
    let result: String = connection.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if result != "ok" {
        return Err(Error::new("store_corrupt", "SQLite integrity failed"));
    }
    Ok(())
}
pub(crate) fn canonical(connection: &Connection, deadline: Option<Instant>) -> Result<()> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    // Table-specific checks include their indexes without consulting FTS shadow
    // tables. The final full-database check runs after rebuilding derived data.
    for table in [
        "meta",
        "collections",
        "collection_policies",
        "documents",
        "revisions",
        "chunks",
        "models",
        "vectors",
        "jobs",
    ] {
        check_deadline(deadline)?;
        let result: String =
            connection.query_row(&format!("PRAGMA integrity_check({table})"), [], |r| {
                r.get(0)
            })?;
        if result != "ok" {
            return Err(Error::new(
                "store_corrupt",
                "Canonical SQLite integrity failed",
            ));
        }
    }
    if version >= 5 {
        let result: String =
            connection.query_row("PRAGMA integrity_check(vector_values)", [], |r| r.get(0))?;
        if result != "ok" {
            return Err(Error::new(
                "store_corrupt",
                "Shared vector table integrity failed",
            ));
        }
        let orphaned: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM vector_values e WHERE NOT EXISTS(SELECT 1 FROM vectors v WHERE v.value_id=e.id))", [], |r| r.get(0))?;
        if orphaned {
            return Err(Error::new("store_corrupt", "Unowned shared vector value"));
        }
    }
    let foreign_keys: i64 =
        connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })?;
    if foreign_keys != 0 {
        return Err(Error::new(
            "store_corrupt",
            "Canonical foreign-key integrity failed",
        ));
    }
    let invalid_flags: i64 = connection.query_row("SELECT count(*) FROM documents WHERE eligible NOT IN (0,1) OR excluded NOT IN (0,1) OR (excluded=1 AND eligible!=0) OR desired<0 OR typeof(desired)!='integer'", [], |r| r.get(0))?;
    if invalid_flags != 0 {
        return Err(Error::new(
            "store_corrupt",
            "Canonical eligibility/desired state is invalid",
        ));
    }
    let generation: Option<String> = connection
        .query_row("SELECT value FROM meta WHERE key='generation'", [], |r| {
            r.get(0)
        })
        .optional()?;
    if generation
        .and_then(|v| v.parse::<i64>().ok())
        .is_none_or(|value| value < 0)
    {
        return Err(Error::new("store_corrupt", "Invalid canonical generation"));
    }
    let mut collections = connection.prepare("SELECT id,name,root FROM collections")?;
    let mut policies = BTreeMap::new();
    let mut rows = collections.query([])?;
    while let Some(row) = rows.next()? {
        check_deadline(deadline)?;
        let id: String = row.get(0)?;
        let name: String = row.get(1)?;
        let root: String = row.get(2)?;
        policies.insert(id.clone(), crate::source_policy::load(connection, &id)?);
        if name.is_empty()
            || name.len() > 256
            || !std::path::Path::new(&root).is_absolute()
            || identity(&["collection", &name, &root]) != id
        {
            return Err(Error::new(
                "store_corrupt",
                "Canonical collection identity mismatch",
            ));
        }
    }
    let mut documents =
        connection.prepare("SELECT id,collection_id,path,eligible FROM documents")?;
    let mut rows = documents.query([])?;
    while let Some(row) = rows.next()? {
        check_deadline(deadline)?;
        let id: String = row.get(0)?;
        let collection: String = row.get(1)?;
        let path: String = row.get(2)?;
        let eligible: bool = row.get(3)?;
        if eligible
            && policies
                .get(&collection)
                .is_some_and(|policy| policy.excludes(&path))
        {
            return Err(Error::new(
                "store_corrupt",
                "Policy-excluded document is eligible",
            ));
        }
        if path.is_empty()
            || std::path::Path::new(&path)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            || identity(&["document", &collection, &path]) != id
        {
            return Err(Error::new(
                "store_corrupt",
                "Canonical document identity mismatch",
            ));
        }
    }
    let invalid_active: i64 = connection.query_row("SELECT count(*) FROM documents d LEFT JOIN revisions r ON r.id=d.active_revision WHERE (d.active_revision IS NOT NULL AND (r.id IS NULL OR r.document_id!=d.id OR d.active_processing IS NULL)) OR (d.status='active' AND d.active_revision IS NULL)", [], |r| r.get(0))?;
    if invalid_active != 0 {
        return Err(Error::new(
            "store_corrupt",
            "Active document revision/processing is invalid",
        ));
    }
    let mut revisions = connection.prepare("SELECT id,document_id,hash,raw FROM revisions")?;
    let mut rows = revisions.query([])?;
    while let Some(row) = rows.next()? {
        check_deadline(deadline)?;
        let revision: String = row.get(0)?;
        let document: String = row.get(1)?;
        let expected: String = row.get(2)?;
        let raw: Vec<u8> = row.get(3)?;
        if hash(&raw) != expected || identity(&["revision", &document, &expected]) != revision {
            return Err(Error::new(
                "store_corrupt",
                "Canonical revision hash/identity mismatch",
            ));
        }
        let text = std::str::from_utf8(&raw)
            .map_err(|_| Error::new("store_corrupt", "Invalid UTF-8 revision"))?;
        let mut chunks = connection
            .prepare("SELECT id,start,end,text,processing FROM chunks WHERE revision_id=?1")?;
        let mut chunks = chunks.query([&revision])?;
        while let Some(chunk) = chunks.next()? {
            check_deadline(deadline)?;
            let id: String = chunk.get(0)?;
            let start: i64 = chunk.get(1)?;
            let end: i64 = chunk.get(2)?;
            let content: String = chunk.get(3)?;
            let processing: String = chunk.get(4)?;
            if start < 0
                || end <= start
                || text.get(start as usize..end as usize) != Some(content.as_str())
                || identity(&[
                    "chunk",
                    &revision,
                    &processing,
                    &start.to_string(),
                    &end.to_string(),
                ]) != id
            {
                return Err(Error::new(
                    "store_corrupt",
                    "Chunk identity or source-span mismatch",
                ));
            }
        }
    }
    let mut spaces = BTreeMap::new();
    let mut models = connection.prepare("SELECT id,manifest FROM models")?;
    for row in models.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        check_deadline(deadline)?;
        let (id, manifest) = row?;
        let manifest: embedding::Manifest = serde_json::from_str(&manifest)
            .map_err(|_| Error::new("store_corrupt", "Invalid embedding manifest"))?;
        if manifest.space_id() != id || manifest.dimensions == 0 || manifest.dimensions > 65536 {
            return Err(Error::new(
                "store_corrupt",
                "Embedding manifest identity/dimensions mismatch",
            ));
        }
        spaces.insert(id, manifest.dimensions);
    }
    let sql = if version >= 5 {
        "SELECT space,data FROM vector_values"
    } else {
        "SELECT space,data FROM vectors"
    };
    let mut vectors = connection.prepare(sql)?;
    let mut rows = vectors.query([])?;
    while let Some(row) = rows.next()? {
        check_deadline(deadline)?;
        let space: String = row.get(0)?;
        let data: Vec<u8> = row.get(1)?;
        if spaces.get(&space).map(|n| n * 4) != Some(data.len())
            || (embedding::dot(&data, &data)? - 1.0).abs() > 0.001
        {
            return Err(Error::new(
                "store_corrupt",
                "Stored vector dimensions/normalization mismatch",
            ));
        }
    }
    if version >= 5 {
        let mut query = connection.prepare("SELECT e.id,e.space,e.text_hash,e.data,c.text FROM vector_values e JOIN vectors v ON v.value_id=e.id JOIN chunks c ON c.id=v.chunk_id")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            check_deadline(deadline)?;
            let id: String = row.get(0)?;
            let space: String = row.get(1)?;
            let text_hash: String = row.get(2)?;
            let data: Vec<u8> = row.get(3)?;
            let text: String = row.get(4)?;
            if hash(text.as_bytes()) != text_hash
                || crate::vector_store::value_id(&space, &text_hash, &data) != id
            {
                return Err(Error::new(
                    "store_corrupt",
                    "Shared vector input/value identity mismatch",
                ));
            }
        }
    }
    Ok(())
}
pub(crate) fn lexical(connection: &Connection, deadline: Option<Instant>) -> Result<()> {
    check_deadline(deadline)?;
    let mismatch:i64=connection.query_row("SELECT (SELECT count(*) FROM lexical l LEFT JOIN chunks c ON c.row_id=l.rowid LEFT JOIN documents d ON d.active_revision=c.revision_id AND d.active_processing=c.processing WHERE d.id IS NULL OR d.eligible=0)+(SELECT count(*) FROM chunks c JOIN documents d ON d.active_revision=c.revision_id AND d.active_processing=c.processing WHERE d.eligible=1 AND NOT EXISTS (SELECT 1 FROM lexical l WHERE l.rowid=c.row_id))",[],|r|r.get(0))?;
    if mismatch != 0 {
        return Err(Error::new("store_corrupt", "FTS visibility check failed"));
    }

    connection.execute(
        "INSERT INTO lexical(lexical) VALUES ('integrity-check')",
        [],
    )?;
    let content_mismatch:i64=connection.query_row("SELECT count(*) FROM lexical l JOIN chunks c ON c.row_id=l.rowid JOIN documents d ON d.active_revision=c.revision_id AND d.active_processing=c.processing WHERE l.text!=c.text OR l.heading!=c.heading OR l.title!=d.path",[],|r|r.get(0))?;
    if content_mismatch != 0 {
        return Err(Error::new(
            "store_corrupt",
            "FTS content disagrees with canonical chunks",
        ));
    }
    let mut rows = connection
        .prepare("SELECT l.identifiers,c.text FROM lexical l JOIN chunks c ON c.row_id=l.rowid")?;
    let mut rows = rows.query([])?;
    while let Some(row) = rows.next()? {
        check_deadline(deadline)?;
        let actual: String = row.get(0)?;
        let text: String = row.get(1)?;
        if actual != crate::store::identifiers(&text) {
            return Err(Error::new(
                "store_corrupt",
                "FTS identifier field disagrees with canonical chunks",
            ));
        }
    }
    Ok(())
}
