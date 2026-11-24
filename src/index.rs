use crate::{
    markdown::{self, identifier_parts, line_offsets, words},
    relevance::{EvidenceQuery, identifier_matches, is_identifier, significant_term},
    source::{Snapshot, is_markdown_path, is_oid, repo_url},
    wire::{NoMatchReason, ReadArgs, ResultStatus},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const SCHEMA_VERSION: i64 = 5;
// Advance this whenever parsing, chunking, or persisted term generation changes.
const PASSAGE_VERSION: i64 = 2;
const DEFAULT_HITS: usize = 5;
const MAX_HITS: usize = 10;
const MAX_EVIDENCE_BYTES: usize = 12 * 1024;
const MAX_READ_LINES: usize = 80;
const MAX_QUERY_BYTES: usize = 2_048;
const SHORT_PASSAGE_BYTES: usize = 240;
const MIN_DETAILED_PASSAGE_BYTES: usize = 150;
const MAX_QUERY_TERMS: usize = u16::BITS as usize;
const MAX_PASSAGE_GAP: usize = 4;
const CONTEXT_PADDING: usize = 12;
const CONTEXT_BYTES: usize = 4 * 1024;
const CONTEXT_CANDIDATES: usize = 16;
const MIN_CONTEXT_TERMS: usize = 3;

const SCHEMA: &str = "
CREATE TABLE kb_revision (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    sha TEXT NOT NULL,
    repo_url TEXT NOT NULL,
    primary_sha TEXT,
    passage_version INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE documents (
    path TEXT PRIMARY KEY NOT NULL,
    blob TEXT NOT NULL,
    title TEXT NOT NULL,
    markdown TEXT NOT NULL,
    source_url TEXT,
    source_revision TEXT
);
";

const PASSAGE_SCHEMA: &str = "
CREATE TABLE passages (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL REFERENCES documents(path) ON DELETE CASCADE,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    start_byte INTEGER NOT NULL,
    byte_count INTEGER NOT NULL,
    heading TEXT NOT NULL
);
CREATE INDEX passage_document ON passages(path);
CREATE VIRTUAL TABLE passage_search USING fts5(
    title, heading, body, terms,
    tokenize = 'porter unicode61', content = '', contentless_delete = 1
);
CREATE TRIGGER delete_passage_search AFTER DELETE ON passages BEGIN
    DELETE FROM passage_search WHERE rowid = old.id;
END;
";

#[derive(Clone, Debug, Serialize)]
pub struct IndexStatus {
    pub indexed_commit: Option<String>,
    pub files: u32,
    pub passages: u32,
    pub retrieval_mode: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Ingested {
    pub commit: String,
    pub files: usize,
    pub updated_files: usize,
    pub removed_files: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Evidence {
    pub title: String,
    pub section: String,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    pub url: String,
    pub markdown: String,
}

impl Evidence {
    fn evidence_bytes(&self) -> usize {
        self.markdown.len() + self.title.len() + self.section.len() + self.url.len()
    }
}

#[derive(Debug, Serialize)]
pub struct Results {
    pub status: ResultStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<NoMatchReason>,
    pub indexed_commit: Option<String>,
    pub hits: Vec<Evidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_hint: Option<ReadArgs>,
}

#[derive(Debug, Serialize)]
pub struct ReadResult {
    pub status: ResultStatus,
    pub indexed_commit: Option<String>,
    pub truncated: bool,
    pub evidence: Option<Evidence>,
}

#[derive(Clone)]
pub struct QueryControl {
    pub cancelled: Arc<AtomicBool>,
    pub deadline: Instant,
}

pub fn initialize(path: &Path) -> Result<Connection> {
    let mut db = Connection::open(path).context("could not open the KB index")?;
    db.busy_timeout(Duration::from_millis(400))?;
    db.pragma_update(None, "foreign_keys", "ON")?;
    let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == 0 {
        // SQLite only enables auto-vacuum before the schema transaction starts.
        db.pragma_update(None, "auto_vacuum", "FULL")?;
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        SCHEMA_VERSION => {}
        0 => {
            tx.execute_batch(SCHEMA)?;
            tx.execute_batch(PASSAGE_SCHEMA)?;
        }
        1 => {
            tx.execute_batch(
                "ALTER TABLE kb_revision ADD COLUMN primary_sha TEXT;
                 ALTER TABLE documents ADD COLUMN source_url TEXT;
                 ALTER TABLE documents ADD COLUMN source_revision TEXT;",
            )?;
        }
        2 => {
            tx.execute_batch(
                "ALTER TABLE documents RENAME COLUMN wiki_url TO source_url;
                 ALTER TABLE documents RENAME COLUMN wiki_sha TO source_revision;",
            )?;
        }
        3 | 4 => {}
        _ => anyhow::bail!(
            "index schema {version} is incompatible; rebuild into a separate database"
        ),
    }
    if (1..4).contains(&version) {
        tx.execute_batch(
            "ALTER TABLE kb_revision ADD COLUMN passage_version INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    if (1..SCHEMA_VERSION).contains(&version) {
        migrate_passages(&tx)?;
    }
    if version != SCHEMA_VERSION {
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    tx.commit()?;
    let mode: String = db.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    db.pragma_update(None, "journal_size_limit", 0)?;
    ensure!(
        mode == "wal",
        "the KB index requires a filesystem with SQLite WAL support"
    );
    Ok(db)
}

fn migrate_passages(db: &Connection) -> Result<()> {
    db.execute_batch(
        "DROP TRIGGER delete_passage_search;
         ALTER TABLE passages RENAME TO old_passages;
         ALTER TABLE passage_search RENAME TO old_passage_search;",
    )?;
    db.execute_batch(PASSAGE_SCHEMA)?;
    {
        let mut documents = db.prepare("SELECT path, markdown FROM documents")?;
        let mut docs = documents.query([])?;
        let mut passages = db.prepare(
            "SELECT p.id, p.start_line, p.end_line, f.title, f.heading, f.body, f.terms
             FROM old_passages p JOIN old_passage_search f ON f.rowid=p.id WHERE p.path=?1",
        )?;
        let mut save_span = db.prepare(
            "INSERT INTO passages(id, path, start_line, end_line, start_byte, byte_count, heading)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        let mut save_index = db.prepare(
            "INSERT INTO passage_search(rowid, title, heading, body, terms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        while let Some(doc) = docs.next()? {
            let path: String = doc.get(0)?;
            let markdown: String = doc.get(1)?;
            let offsets = line_offsets(&markdown);
            let mut spans = passages.query([&path])?;
            while let Some(row) = spans.next()? {
                let id: i64 = row.get(0)?;
                let start = usize::try_from(row.get::<_, i64>(1)?)?;
                let end = usize::try_from(row.get::<_, i64>(2)?)?;
                let first = start.checked_sub(1).and_then(|line| offsets.get(line));
                let (Some(&first), Some(&last)) = (first, offsets.get(end)) else {
                    anyhow::bail!("invalid legacy passage lines");
                };
                let title: String = row.get(3)?;
                let heading: String = row.get(4)?;
                let body: String = row.get(5)?;
                let terms: String = row.get(6)?;
                ensure!(
                    !body.is_empty() && markdown.get(first..last) == Some(body.as_str()),
                    "legacy passage does not match its document"
                );
                save_span.execute(params![
                    id,
                    path,
                    start as i64,
                    end as i64,
                    first as i64,
                    body.len() as i64,
                    heading
                ])?;
                save_index.execute(params![id, title, heading, body, terms])?;
            }
        }
    }
    let complete = db.query_row(
        "SELECT (SELECT COUNT(*) FROM passages) = (SELECT COUNT(*) FROM old_passages)",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    ensure!(complete, "legacy index is missing passages");
    db.execute_batch("DROP TABLE old_passage_search; DROP TABLE old_passages;")?;
    Ok(())
}

pub fn reader(path: &Path, control: Option<QueryControl>) -> Result<Connection> {
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("could not open the KB index")?;
    db.busy_timeout(Duration::from_millis(200))?;
    let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure!(
        version == SCHEMA_VERSION,
        "index schema {version} is incompatible"
    );
    if let Some(control) = control {
        db.progress_handler(
            1_000,
            Some(move || {
                control.cancelled.load(Ordering::Relaxed) || Instant::now() >= control.deadline
            }),
        )?;
    }
    Ok(db)
}

pub fn revision(db: &Connection) -> Result<Option<(String, String)>> {
    Ok(db
        .query_row(
            "SELECT sha, repo_url FROM kb_revision WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

fn source_revision(db: &Connection) -> Result<Option<(String, String, String)>> {
    Ok(db
        .query_row(
            "SELECT sha, repo_url, COALESCE(primary_sha, sha) FROM kb_revision WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?)
}

struct IndexDocument<'a> {
    path: String,
    markdown_path: &'a str,
    markdown: &'a str,
    hash: &'a str,
    title: Option<&'a str>,
    url: Option<String>,
    revision: Option<&'a str>,
}

pub fn ingest(db: &mut Connection, source: &Snapshot) -> Result<Ingested> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let reuse_passages = tx
        .query_row(
            "SELECT passage_version FROM kb_revision WHERE id=1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        == Some(PASSAGE_VERSION);
    let existing = {
        let mut statement = tx.prepare("SELECT path, blob, title FROM documents")?;
        statement
            .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
            .collect::<rusqlite::Result<HashMap<String, (String, String)>>>()?
    };
    let mut documents = source
        .files
        .iter()
        .map(|doc| IndexDocument {
            path: doc.path.clone(),
            markdown_path: &doc.path,
            markdown: &doc.markdown,
            hash: &doc.blob,
            title: None,
            url: None,
            revision: None,
        })
        .collect::<Vec<_>>();
    let project = repo_url(&source.repo_url)?;
    for wiki in &source.wikis {
        for doc in &wiki.files {
            documents.push(IndexDocument {
                path: wiki.path(&doc.path),
                markdown_path: &doc.path,
                markdown: &doc.markdown,
                hash: &doc.blob,
                title: None,
                url: Some(wiki.citation(&project, &doc.path)?),
                revision: Some(&wiki.commit),
            });
        }
    }
    documents.extend(source.shortcut_docs.iter().map(|page| IndexDocument {
        path: page.path(),
        markdown_path: &page.doc.title,
        markdown: &page.doc.content_markdown,
        hash: &page.sha256,
        title: Some(&page.doc.title),
        url: Some(page.citation(source.shortcut_workspace.as_deref())),
        revision: Some(&page.sha256),
    }));
    let current: HashSet<&str> = documents.iter().map(|doc| doc.path.as_str()).collect();
    let mut removed_files = 0;
    for path in existing
        .keys()
        .filter(|path| !current.contains(path.as_str()))
    {
        tx.execute("DELETE FROM documents WHERE path=?1", [path])?;
        removed_files += 1;
    }
    let mut updated_files = 0;
    for doc in &documents {
        let path = &doc.path;
        if reuse_passages
            && existing.get(path).is_some_and(|(hash, title)| {
                hash == doc.hash && doc.title.is_none_or(|label| label == title)
            })
        {
            tx.execute(
                "UPDATE documents SET source_url=?2, source_revision=?3 WHERE path=?1
                 AND (source_url IS NOT ?2 OR source_revision IS NOT ?3)",
                params![path, doc.url, doc.revision],
            )?;
            continue;
        }
        let (title, passages) = markdown::parse(doc.markdown_path, doc.markdown);
        let title = doc.title.unwrap_or(&title);
        tx.execute("DELETE FROM documents WHERE path=?1", [path])?;
        tx.execute(
            "INSERT INTO documents(path, blob, title, markdown, source_url, source_revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![path, doc.hash, title, doc.markdown, doc.url, doc.revision],
        )?;
        for passage in passages {
            tx.execute(
                "INSERT INTO passages(path, start_line, end_line, start_byte, byte_count, heading)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    path,
                    passage.start_line as i64,
                    passage.end_line as i64,
                    passage.start_byte as i64,
                    passage.body.len() as i64,
                    passage.heading
                ],
            )?;
            let terms = identifier_parts(passage.body);
            tx.execute(
                "INSERT INTO passage_search(rowid, title, heading, body, terms) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![tx.last_insert_rowid(), title, passage.heading, passage.body, terms],
            )?;
        }
        updated_files += 1;
    }
    let commit = source.revision();
    tx.execute(
        "INSERT INTO kb_revision(id, sha, repo_url, primary_sha, passage_version) VALUES (1, ?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET sha=excluded.sha, repo_url=excluded.repo_url,
             primary_sha=excluded.primary_sha, passage_version=excluded.passage_version
         WHERE sha IS NOT excluded.sha OR repo_url IS NOT excluded.repo_url
             OR primary_sha IS NOT excluded.primary_sha OR passage_version IS NOT excluded.passage_version",
        params![commit, source.repo_url, source.commit, PASSAGE_VERSION],
    )?;
    tx.commit()?;
    Ok(Ingested {
        commit,
        files: documents.len(),
        updated_files,
        removed_files,
    })
}

pub fn status(db: &mut Connection) -> Result<IndexStatus> {
    let tx = db.transaction()?;
    Ok(IndexStatus {
        indexed_commit: revision(&tx)?.map(|(commit, _)| commit),
        files: tx.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))?,
        passages: tx.query_row("SELECT COUNT(*) FROM passages", [], |row| row.get(0))?,
        retrieval_mode: "lexical",
    })
}

#[derive(Debug)]
struct Query {
    terms: Vec<String>,
    normalized: Vec<String>,
    identifiers: Vec<String>,
    aliases: Vec<String>,
}

impl Query {
    fn new(query: &str) -> Result<Self> {
        ensure!(
            !query.trim().is_empty() && query.len() <= MAX_QUERY_BYTES,
            "query must be 1–2048 bytes"
        );
        let mut terms = Vec::new();
        let mut stems = Vec::new();
        let mut identifiers = Vec::new();
        let mut aliases = Vec::new();
        for word in words(query).filter(|word| word.len() <= 128) {
            let normalized = word.to_lowercase();
            let Some(stem) = significant_term(&normalized).map(Cow::into_owned) else {
                continue;
            };
            if is_identifier(word) && !identifiers.contains(&normalized) {
                identifiers.push(normalized.clone());
                let parts = identifier_parts(word);
                if !parts.is_empty() {
                    aliases.push(parts.trim().to_lowercase());
                }
            }
            if !stems.contains(&stem) {
                terms.push(normalized);
                stems.push(stem);
            }
            if terms.len() == MAX_QUERY_TERMS {
                break;
            }
        }
        Ok(Self {
            terms,
            normalized: stems,
            identifiers,
            aliases,
        })
    }

    fn expression(&self) -> String {
        self.terms
            .iter()
            .chain(&self.aliases)
            .map(|term| format!("\"{term}\""))
            .collect::<Vec<_>>()
            .join(" OR ")
    }

    fn grade(&self, body: &str, heading: &str, title: &str, wiki_owner: &str) -> Option<usize> {
        let exact = |key: &str| {
            identifier_matches(body, key)
                || identifier_matches(heading, key)
                || identifier_matches(title, key)
        };
        let exact_count = self.identifiers.iter().filter(|key| exact(key)).count();
        if self.terms.len() >= 3 && exact_count == 0 {
            let terms = [heading, title, body].into_iter().flat_map(words);
            if self.match_terms(terms, 2).count_ones() < 2 {
                return None;
            }
        }
        // A wiki already belongs to its owning project or group. Give its relevant
        // passages the same identifier credit as a passage that repeats the owner.
        let owner_matches = if wiki_owner.is_empty() {
            0
        } else {
            self.identifiers
                .iter()
                .filter(|word| identifier_matches(wiki_owner, word) && !exact(word))
                .count()
        };
        Some(exact_count + owner_matches)
    }

    fn body_matches(&self, body: &str) -> u16 {
        self.match_terms(words(body), self.normalized.len() as u32)
    }

    fn match_terms<'a>(&self, terms: impl Iterator<Item = &'a str>, required: u32) -> u16 {
        let mut matches = 0u16;
        for word in terms.filter(|word| (2..=128).contains(&word.len())) {
            let lower = if word
                .bytes()
                .all(|byte| byte.is_ascii() && !byte.is_ascii_uppercase())
            {
                Cow::Borrowed(word)
            } else {
                Cow::Owned(word.to_lowercase())
            };
            let Some(stem) = significant_term(&lower) else {
                continue;
            };
            if let Some(index) = self
                .normalized
                .iter()
                .position(|term| term == stem.as_ref())
            {
                matches |= 1 << index;
                if matches.count_ones() >= required {
                    break;
                }
            }
        }
        matches
    }

    fn promote_detailed_hit(&self, hits: &mut [(usize, &Evidence)]) {
        let Some((first_exact, first)) = hits.first() else {
            return;
        };
        if hits.len() < 2 || self.terms.len() < 2 || first.markdown.len() > SHORT_PASSAGE_BYTES {
            return;
        }
        let first_terms = self.body_matches(&first.markdown);
        let minimum_bytes = MIN_DETAILED_PASSAGE_BYTES.max(first.markdown.len() * 2);
        let mut best = 0;
        let mut best_count = first_terms.count_ones().max(1);
        for (index, (exact, hit)) in hits.iter().enumerate().skip(1).take(DEFAULT_HITS - 1) {
            if exact != first_exact || hit.path != first.path || hit.markdown.len() < minimum_bytes
            {
                continue;
            }
            let terms = self.body_matches(&hit.markdown);
            let count = terms.count_ones();
            if terms & first_terms == first_terms && count > best_count {
                best = index;
                best_count = count;
            }
        }
        hits.swap(0, best);
    }
}

fn combine_adjacent_hit(
    db: &Connection,
    query: &Query,
    evidence: &EvidenceQuery,
    hits: &mut Vec<Evidence>,
) -> Result<()> {
    let Some(first) = hits.first() else {
        return Ok(());
    };
    if hits.len() < 2 {
        return Ok(());
    }
    let mut first_terms = None;
    let neighbor = hits
        .iter()
        .enumerate()
        .skip(1)
        .filter_map(|(rank, hit)| {
            if hit.path != first.path {
                return None;
            }
            let start = first.start_line.min(hit.start_line);
            let end = first.end_line.max(hit.end_line);
            let gap = first
                .start_line
                .max(hit.start_line)
                .saturating_sub(first.end_line.min(hit.end_line));
            if gap == 0 || gap > MAX_PASSAGE_GAP + 1 || end - start >= markdown::PASSAGE_LINES {
                return None;
            }
            let covered = *first_terms.get_or_insert_with(|| query.body_matches(&first.markdown));
            let added = (query.body_matches(&hit.markdown) & !covered).count_ones();
            (added > 0).then_some((rank, start, end, gap, added))
        })
        .max_by_key(|&(rank, _, _, gap, added)| {
            (added, std::cmp::Reverse(gap), std::cmp::Reverse(rank))
        });
    let Some((rank, start, end, _, _)) = neighbor else {
        return Ok(());
    };
    let source: String = db.query_row(
        "SELECT markdown FROM documents WHERE path=?1",
        [&first.path],
        |row| row.get(0),
    )?;
    let offsets = line_offsets(&source);
    let end_byte = *offsets
        .get(end)
        .context("indexed passage end is outside its document")?;
    let body = &source[offsets[start - 1]..end_byte];
    if body.len() > markdown::PASSAGE_BYTES {
        return Ok(());
    }
    let neighbor = &hits[rank];
    let combined = Evidence {
        title: first.title.clone(),
        section: if first.section == neighbor.section {
            first.section.clone()
        } else {
            first.title.clone()
        },
        path: first.path.clone(),
        start_line: start,
        end_line: end,
        commit: first.commit.clone(),
        source_commit: first.source_commit.clone(),
        source_revision: first.source_revision.clone(),
        url: match first.url.split_once('#') {
            Some((url, _)) => format!("{url}#{}", line_anchor(start, end)),
            None => first.url.clone(),
        },
        markdown: body.to_owned(),
    };
    let mut merged = vec![combined];
    merged.extend(
        hits.iter()
            .skip(1)
            .filter(|hit| hit.path != first.path || hit.start_line < start || hit.end_line > end)
            .cloned(),
    );
    if merged.iter().map(Evidence::evidence_bytes).sum::<usize>() <= MAX_EVIDENCE_BYTES
        && evidence.accepts(&merged)
    {
        *hits = merged;
    }
    Ok(())
}

fn extend_context_range(
    query: &EvidenceQuery,
    source: &str,
    first: (usize, usize),
    pivots: &[(usize, usize)],
) -> Option<(usize, usize)> {
    let offsets = line_offsets(source);
    let last_line = offsets.len() - 1;
    let start = first.0.saturating_sub(CONTEXT_PADDING).max(1);
    let end = (start + MAX_READ_LINES - 1)
        .min(first.1.saturating_add(CONTEXT_PADDING))
        .min(last_line);
    if end < start {
        return None;
    }
    let baseline_start = *offsets.get(start - 1)?;
    let baseline_end = *offsets.get(end)?;
    if baseline_end.saturating_sub(baseline_start) >= CONTEXT_BYTES {
        return None;
    }

    let mut best = None;
    let mut best_score = (0, std::cmp::Reverse(0));
    let mut checked = HashSet::new();
    for &(pivot_start, pivot_end) in pivots {
        for padding in [0, 4, CONTEXT_PADDING] {
            let from = start.min(pivot_start.saturating_sub(padding).max(1));
            let to = end.max(pivot_end.saturating_add(padding)).min(last_line);
            if (from, to) == (start, end)
                || to - from >= MAX_READ_LINES
                || !checked.insert((from, to))
            {
                continue;
            }
            let length = offsets[to] - offsets[from - 1];
            if length > CONTEXT_BYTES {
                continue;
            }
            let terms = query.context_term_count([
                &source[offsets[from - 1]..baseline_start],
                &source[baseline_end..offsets[to]],
            ]);
            let score = (terms, std::cmp::Reverse(length));
            if terms >= MIN_CONTEXT_TERMS && score > best_score {
                best = Some((from, to));
                best_score = score;
            }
        }
    }
    best
}

fn suggest_context(
    db: &Connection,
    query: &EvidenceQuery,
    hits: &[Evidence],
    ranked: &[(usize, Evidence)],
) -> Result<Option<ReadArgs>> {
    let Some(first) = hits.first() else {
        return Ok(None);
    };
    let pivots: Vec<_> = hits
        .iter()
        .chain(ranked.iter().take(CONTEXT_CANDIDATES).map(|(_, hit)| hit))
        .filter(|hit| hit.path == first.path)
        .map(|hit| (hit.start_line, hit.end_line))
        .filter(|&(start, end)| start < first.start_line || end > first.end_line)
        .collect();
    if pivots.is_empty() {
        return Ok(None);
    }
    let source: String = db.query_row(
        "SELECT markdown FROM documents WHERE path=?1",
        [&first.path],
        |row| row.get(0),
    )?;
    Ok(
        extend_context_range(query, &source, (first.start_line, first.end_line), &pivots)
            .filter(|&(start, end)| {
                let new_lines = source
                    .lines()
                    .enumerate()
                    .skip(start - 1)
                    .take(end - start + 1)
                    .filter(|(index, _)| {
                        let line = index + 1;
                        !hits.iter().any(|hit| {
                            hit.path == first.path
                                && (hit.start_line..=hit.end_line).contains(&line)
                        })
                    })
                    .map(|(_, text)| text);
                query.context_term_count(new_lines) > 0
            })
            .map(|(start_line, end_line)| ReadArgs {
                path: first.path.clone(),
                commit: first.commit.clone(),
                start_line,
                end_line,
            }),
    )
}

pub fn search(db: &mut Connection, query: &str, limit: Option<usize>) -> Result<Results> {
    validate_search(query, limit)?;
    let limit = limit.unwrap_or(DEFAULT_HITS);
    let original_query = query;
    let query = Query::new(query)?;
    let tx = db.transaction()?;
    let Some((commit, repo, primary)) = source_revision(&tx)? else {
        return Ok(Results {
            status: ResultStatus::Unavailable,
            reason: None,
            indexed_commit: None,
            hits: Vec::new(),
            context_hint: None,
        });
    };
    let base = repo_url(&repo)?;
    let mut hits = Vec::new();
    let mut had_candidates = false;
    if !query.terms.is_empty() {
        let mut statement = tx.prepare(
            "WITH ranked AS MATERIALIZED (
                 SELECT p.path, p.heading, p.start_line, p.end_line, p.start_byte, p.byte_count,
                        bm25(passage_search, 3.0, 3.0, 1.0, 0.35) AS score
                 FROM passage_search JOIN passages p ON p.id=passage_search.rowid
                 WHERE passage_search MATCH ?1
                 ORDER BY score, p.path, p.start_line
                 LIMIT 64
             )
             SELECT d.title, r.heading, r.path, r.start_line, r.end_line,
                    CAST(substr(CAST(d.markdown AS BLOB), r.start_byte + 1, r.byte_count) AS TEXT),
                    d.source_url, d.source_revision
             FROM ranked r JOIN documents d ON d.path=r.path
             ORDER BY r.score, r.path, r.start_line",
        )?;
        let mut rows = statement.query([query.expression()])?;
        while let Some(row) = rows.next()? {
            had_candidates = true;
            let title: String = row.get(0)?;
            let section: String = row.get(1)?;
            let path: String = row.get(2)?;
            let start_line = usize::try_from(row.get::<_, i64>(3)?)?;
            let end_line = usize::try_from(row.get::<_, i64>(4)?)?;
            let markdown: String = row.get(5)?;
            let source_url: Option<String> = row.get(6)?;
            let wiki_owner = source_url
                .as_deref()
                .and_then(|url| url.split_once("/-/wikis/"))
                .and_then(|(root, _)| root.rsplit('/').next())
                .unwrap_or_default();
            if let Some(exact) = query.grade(&markdown, &section, &title, wiki_owner) {
                let url = match source_url {
                    Some(url) => url,
                    None => citation(&base, &primary, &path, start_line, end_line)?,
                };
                let revision = row
                    .get::<_, Option<String>>(7)?
                    .unwrap_or_else(|| primary.clone());
                let (source_commit, source_revision) = source_identity(&path, revision);
                hits.push((
                    exact,
                    Evidence {
                        title,
                        section,
                        path,
                        start_line,
                        end_line,
                        commit: commit.clone(),
                        source_commit,
                        source_revision,
                        url,
                        markdown,
                    },
                ));
            }
        }
    }
    let evidence = EvidenceQuery::new(original_query);
    if evidence.asks_for_procedure() {
        let mut source_matches = HashMap::new();
        for (exact, hit) in &hits {
            if *exact > 0 {
                let best = source_matches.entry(hit.path.clone()).or_insert(0);
                *best = (*best).max(*exact);
            }
        }
        hits.sort_by_cached_key(|(exact, hit)| {
            let procedure = evidence.looks_like_procedure(hit);
            let exact = if procedure {
                // A procedure section often omits a service name already given in its document.
                source_matches.get(&hit.path).copied().unwrap_or(*exact)
            } else {
                *exact
            };
            std::cmp::Reverse((exact, procedure))
        });
    } else {
        hits.sort_by_key(|(exact, _)| std::cmp::Reverse(*exact));
    }
    let mut bytes = 0;
    let ranked = hits;
    let mut selected: Vec<_> = ranked
        .iter()
        .filter_map(|(exact, hit)| {
            let size = hit.evidence_bytes();
            if bytes + size > MAX_EVIDENCE_BYTES {
                return None;
            }
            bytes += size;
            Some((*exact, hit))
        })
        .take(limit.max(DEFAULT_HITS))
        .collect();
    // Apply the same first-hit ranking for a one-result request and the default shortlist.
    query.promote_detailed_hit(&mut selected);
    let mut hits: Vec<_> = selected
        .into_iter()
        .take(limit)
        .map(|(_, hit)| hit.clone())
        .collect();
    let mut insufficient = had_candidates && hits.is_empty();
    let mut context_hint = None;
    if !hits.is_empty() {
        insufficient = !evidence.accepts(&hits);
        if insufficient {
            hits.clear();
        } else {
            combine_adjacent_hit(&tx, &query, &evidence, &mut hits)?;
            if query.terms.len() >= MIN_CONTEXT_TERMS {
                context_hint = suggest_context(&tx, &evidence, &hits, &ranked)?;
            }
        }
    }
    Ok(Results {
        status: if hits.is_empty() {
            ResultStatus::NoMatch
        } else {
            ResultStatus::Ok
        },
        reason: if insufficient {
            Some(NoMatchReason::InsufficientEvidence)
        } else if hits.is_empty() {
            Some(NoMatchReason::NoRankedMatches)
        } else {
            None
        },
        indexed_commit: Some(commit),
        hits,
        context_hint,
    })
}

pub fn validate_search(query: &str, limit: Option<usize>) -> Result<()> {
    ensure!(
        !query.trim().is_empty() && query.len() <= MAX_QUERY_BYTES,
        "query must be 1–2048 bytes"
    );
    ensure!(
        (1..=MAX_HITS).contains(&limit.unwrap_or(DEFAULT_HITS)),
        "limit must be 1–10"
    );
    Ok(())
}

pub fn validate_read(path: &str, commit: &str, start: usize, end: usize) -> Result<()> {
    ensure!(
        is_markdown_path(path) && is_oid(commit),
        "invalid indexed path or commit"
    );
    ensure!(
        start > 0 && end >= start && end - start < MAX_READ_LINES,
        "read range must contain 1–80 lines"
    );
    Ok(())
}

pub fn read(
    db: &mut Connection,
    path: &str,
    commit: &str,
    start: usize,
    end: usize,
) -> Result<ReadResult> {
    validate_read(path, commit, start, end)?;
    let tx = db.transaction()?;
    let Some((current, repo, primary)) = source_revision(&tx)? else {
        return Ok(ReadResult {
            status: ResultStatus::Unavailable,
            indexed_commit: None,
            truncated: false,
            evidence: None,
        });
    };
    let empty = |status| ReadResult {
        status,
        indexed_commit: Some(current.clone()),
        truncated: false,
        evidence: None,
    };
    if current != commit {
        return Ok(empty(ResultStatus::StaleRevision));
    }
    let source = tx
        .query_row(
            "SELECT title, markdown, source_url, source_revision FROM documents WHERE path=?1",
            [path],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((title, text, source_url, source_revision)) = source else {
        return Ok(empty(ResultStatus::NotFound));
    };
    let offsets = line_offsets(&text);
    if start >= offsets.len() {
        return Ok(empty(ResultStatus::RangeOutOfBounds));
    }
    let mut actual_end = start;
    let end_limit = end.min(offsets.len() - 1);
    while actual_end < end_limit
        && offsets[actual_end + 1] - offsets[start - 1] <= MAX_EVIDENCE_BYTES
    {
        actual_end += 1;
    }
    let section = tx
        .query_row(
            "SELECT heading FROM passages
         WHERE passages.path=?1 AND start_line<=?2 AND end_line>=?2
         ORDER BY start_line DESC LIMIT 1",
            params![path, start as i64],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_default();
    let url = match source_url {
        Some(url) => url,
        None => citation(&repo_url(&repo)?, &primary, path, start, actual_end)?,
    };
    let (source_commit, source_revision) =
        source_identity(path, source_revision.unwrap_or(primary));
    let evidence = Evidence {
        url,
        title,
        section,
        path: path.to_owned(),
        start_line: start,
        end_line: actual_end,
        commit: commit.to_owned(),
        source_commit,
        source_revision,
        markdown: text[offsets[start - 1]..offsets[actual_end]].to_owned(),
    };
    Ok(ReadResult {
        status: ResultStatus::Ok,
        indexed_commit: Some(current),
        truncated: actual_end != end,
        evidence: Some(evidence),
    })
}

fn source_identity(path: &str, revision: String) -> (Option<String>, Option<String>) {
    if path.starts_with(crate::shortcut::PATH_PREFIX) {
        (None, Some(revision))
    } else {
        (Some(revision), None)
    }
}

fn citation(repo: &url::Url, commit: &str, path: &str, start: usize, end: usize) -> Result<String> {
    let mut url = repo.clone();
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid GitLab project URL"))?
        .pop_if_empty()
        .extend(["-", "blob", commit])
        .extend(path.split('/'));
    url.set_fragment(Some(&line_anchor(start, end)));
    Ok(url.into())
}

fn line_anchor(start: usize, end: usize) -> String {
    if start == end {
        format!("L{start}")
    } else {
        format!("L{start}-{end}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_v4_passages_to_utf8_ranges_without_copying_markdown() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite");
        let commit = "a".repeat(40);
        let body = "The Kestrel sail is named brise-été.\r\n";
        let markdown = format!("# Équipement\r\n\r\n{body}");
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(
                "PRAGMA user_version=4;
                 CREATE TABLE kb_revision(id INTEGER PRIMARY KEY, sha TEXT, repo_url TEXT,
                     primary_sha TEXT, passage_version INTEGER);
                 CREATE TABLE documents(path TEXT PRIMARY KEY, blob TEXT, title TEXT,
                     markdown TEXT, source_url TEXT, source_revision TEXT);
                 CREATE TABLE passages(id INTEGER PRIMARY KEY, path TEXT REFERENCES documents(path)
                     ON DELETE CASCADE, start_line INTEGER, end_line INTEGER);
                 CREATE INDEX passages_by_path ON passages(path);
                 CREATE VIRTUAL TABLE passage_search USING fts5(
                     title, heading, body, terms, tokenize='porter unicode61');
                 CREATE TRIGGER delete_passage_search AFTER DELETE ON passages BEGIN
                     DELETE FROM passage_search WHERE rowid=old.id;
                 END;",
            )
            .unwrap();
            db.execute(
                "INSERT INTO kb_revision VALUES(1, ?1, 'https://gitlab.example.org/test/kb', ?1, 2)",
                [&commit],
            )
            .unwrap();
            db.execute(
                "INSERT INTO documents VALUES('sail.md', ?1, 'Équipement', ?2, NULL, NULL)",
                params![commit, markdown],
            )
            .unwrap();
            db.execute("INSERT INTO passages VALUES(3, 'sail.md', 3, 3)", [])
                .unwrap();
            db.execute(
                "INSERT INTO passage_search(rowid, title, heading, body, terms)
                 VALUES(3, 'Équipement', 'Équipement', ?1, '')",
                [body],
            )
            .unwrap();
        }
        let mut db = initialize(&path).unwrap();
        let result = search(&mut db, "Kestrel", None).unwrap();
        assert_eq!(result.hits[0].markdown, body);
        assert_eq!(result.hits[0].start_line, 3);
        let span = read(&mut db, "sail.md", &commit, 3, 3)
            .unwrap()
            .evidence
            .unwrap();
        assert_eq!(span.section, "Équipement");
        assert_eq!(span.markdown, body);
        let copies: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE name='passage_search_content'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(copies, 0, "FTS should only persist the full-text index");
    }

    #[test]
    fn follow_up_extends_the_original_line_window_within_the_same_budget() {
        let query = EvidenceQuery::new("WidgetService request routing correlation");
        let mut lines = vec!["Background.\n".to_owned(); 40];
        lines[25] = "WidgetService uses request correlation for routing.\n".into();
        let pivots = [(26, 26), (34, 35)];
        let source = lines.concat();
        assert_eq!(
            extend_context_range(&query, &source, (8, 9), &pivots),
            Some((1, 26))
        );

        for line in &mut lines[21..25] {
            *line = format!("{}\n", "filler ".repeat(200));
        }
        assert_eq!(
            extend_context_range(&query, &lines.concat(), (8, 9), &pivots),
            None
        );
    }
}
