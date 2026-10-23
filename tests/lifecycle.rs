use know_things::{
    contracts::*,
    embedding::{Embedder, Manifest},
    store::Store,
};
use std::{collections::BTreeMap, fs};
use tempfile::TempDir;
mod support;

struct Fixture {
    manifest: Manifest,
}
impl Fixture {
    fn new(name: &str) -> Self {
        Self {
            manifest: Manifest {
                name: name.into(),
                artifacts: BTreeMap::new(),
                tokenizer: "fixture".into(),
                architecture: "fixture-only".into(),
                quantization: "f32".into(),
                runtime: "test".into(),
                query_prefix: "".into(),
                document_prefix: "".into(),
                pooling: "fixture".into(),
                normalization: "l2".into(),
                dimensions: 2,
                max_input_tokens: 4096,
                license: "test fixture".into(),
            },
        }
    }
}
impl Embedder for Fixture {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    fn embed(&mut self, text: &str, _: bool, _: std::time::Instant) -> Result<Vec<f32>> {
        Ok(if text.contains("automobile") || text.contains("car") {
            vec![1., 0.]
        } else {
            vec![0., 1.]
        })
    }
}
fn query(q: &str) -> Search {
    Search {
        query: q.into(),
        collections: vec!["kb".into()],
        top_k: 20,
        evidence_token_budget: 65536,
        deadline_ms: 5000,
        mode: Mode::Hybrid,
        strict_hybrid: false,
    }
}
fn setup(model: bool) -> (TempDir, Store) {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("source")).unwrap();
    let mut store = Store::open(
        &temp.path().join("data"),
        if model {
            Some(Box::new(Fixture::new("a")))
        } else {
            None
        },
    )
    .unwrap();
    store.register("kb", &temp.path().join("source")).unwrap();
    (temp, store)
}
fn ingest(store: &mut Store, path: &str, text: &str) {
    let staged = store.stage("kb", path, text).unwrap().unwrap();
    assert!(store.activate(staged).unwrap());
}
#[test]
fn exact_references_survive_edits_but_not_exclusion() {
    let (_temp, mut store) = setup(false);
    ingest(
        &mut store,
        "guide.md",
        "# Guide\r\n\r\nOld Café redis-cache.DotName policy.\r\n",
    );
    let pack = store.search(query("redis-cache.DotName")).unwrap();
    let old = pack
        .evidence
        .iter()
        .find(|e| e.text.contains("Old"))
        .unwrap()
        .clone();
    assert_eq!(store.context(&old.reference).unwrap(), old.text);
    ingest(
        &mut store,
        "guide.md",
        "# Guide\n\nNew redis-cache.DotName policy.\n",
    );
    let new = store.search(query("redis-cache.DotName")).unwrap();
    assert!(new.evidence.iter().all(|e| !e.text.contains("Old")));
    assert_eq!(store.context(&old.reference).unwrap(), old.text);
    let mut forged = old.reference.clone();
    forged.content_hash = "forged".into();
    assert_eq!(store.context(&forged).unwrap_err().code, "invalid_request");
    store.exclude(&old.reference.document_id).unwrap();
    assert!(store.search(query("policy")).unwrap().evidence.is_empty());
    assert_eq!(
        store.context(&old.reference).unwrap_err().code,
        "source_unavailable"
    );
    store.integrity().unwrap();
}
#[test]
fn dense_candidates_are_independent_of_lexical_candidates() {
    let (_temp, mut store) = setup(true);
    ingest(
        &mut store,
        "car.md",
        "An automobile needs annual maintenance.",
    );
    ingest(
        &mut store,
        "distractor.md",
        "The car acronym here refers to something unrelated.",
    );
    let mut q = query("car");
    q.top_k = 20;
    q.strict_hybrid = true;
    let p = store.search(q).unwrap();
    assert_eq!(p.actual_mode, Mode::Hybrid);
    assert!(p.evidence.iter().any(|e| e.source == "car.md"));
}
#[test]
fn late_jobs_cannot_resurrect_or_overwrite_sources() {
    let (_temp, mut store) = setup(false);
    ingest(&mut store, "a.md", "original");
    let old = store.stage("kb", "a.md", "obsolete").unwrap().unwrap();
    let newest = store.stage("kb", "a.md", "latest").unwrap().unwrap();
    assert!(store.activate(newest).unwrap());
    assert!(!store.activate(old).unwrap());
    let late = store.stage("kb", "a.md", "resurrection").unwrap().unwrap();
    store.exclude(late.document_id()).unwrap();
    assert!(!store.activate(late).unwrap());
    assert!(
        store
            .search(query("resurrection"))
            .unwrap()
            .evidence
            .is_empty()
    );
    store.integrity().unwrap();
}
#[test]
fn interrupted_staging_preserves_old_revision_after_restart() {
    let (temp, mut store) = setup(false);
    ingest(&mut store, "a.md", "old state");
    let _ = store.stage("kb", "a.md", "new state").unwrap();
    let generation = store.generation().unwrap();
    drop(store);
    let mut store = Store::open(&temp.path().join("data"), None).unwrap();
    assert_eq!(store.generation().unwrap(), generation);
    assert!(
        store
            .search(query("old"))
            .unwrap()
            .evidence
            .iter()
            .any(|e| e.text == "old state")
    );
    store.integrity().unwrap();
}
#[test]
fn scope_and_budget_are_enforced_with_unicode() {
    let (temp, mut store) = setup(false);
    fs::create_dir(temp.path().join("other")).unwrap();
    store
        .register("private", &temp.path().join("other"))
        .unwrap();
    ingest(
        &mut store,
        "a.md",
        "Café policy is long enough for an excerpt.",
    );
    let staged = store
        .stage("private", "secret.md", "secret policy")
        .unwrap()
        .unwrap();
    store.activate(staged).unwrap();
    let mut q = query("policy");
    q.evidence_token_budget = 5;
    let p = store.search(q).unwrap();
    assert!(p.truncated);
    assert!(p.token_count <= 5);
    assert!(p.evidence.iter().all(|e| e.source != "secret.md"));
    for e in p.evidence {
        assert_eq!(store.context(&e.reference).unwrap(), e.text);
    }
    let mut q = query("policy");
    q.collections.clear();
    assert_eq!(store.search(q).unwrap_err().code, "invalid_request");
}
#[test]
fn absent_or_incomplete_embeddings_are_explicit() {
    let (temp, mut store) = setup(false);
    ingest(&mut store, "a.md", "policy");
    let p = store.search(query("policy")).unwrap();
    assert_eq!(p.actual_mode, Mode::Lexical);
    assert!(!p.degradation_reasons.is_empty());
    let mut q = query("policy");
    q.strict_hybrid = true;
    assert_eq!(store.search(q).unwrap_err().code, "model_unavailable");
    drop(store);
    let mut store =
        Store::open(&temp.path().join("data"), Some(Box::new(Fixture::new("b")))).unwrap();
    assert_eq!(
        store.search(query("policy")).unwrap().actual_mode,
        Mode::Lexical
    );
    let staged = store.stage("kb", "a.md", "policy").unwrap().unwrap();
    store.activate(staged).unwrap();
    assert_eq!(
        store.search(query("policy")).unwrap().actual_mode,
        Mode::Hybrid
    );
}
#[test]
fn reconcile_removes_missing_and_invalid_sources_without_following_links() {
    let (temp, mut store) = setup(false);
    let source = temp.path().join("source");
    fs::write(source.join("a.md"), "policy original").unwrap();
    fs::write(source.join("b.pdf"), "unsupported").unwrap();
    let p = store.reconcile("kb").unwrap();
    assert_eq!(p.active, 1);
    assert_eq!(p.failures.len(), 1);
    let old = store.search(query("policy")).unwrap().evidence[0]
        .reference
        .clone();
    fs::remove_file(source.join("a.md")).unwrap();
    store.reconcile("kb").unwrap();
    assert!(store.context(&old).is_err());
    fs::write(source.join("a.md"), "policy restored").unwrap();
    assert_eq!(store.reconcile("kb").unwrap().active, 1);
    let outside = temp.path().join("outside.md");
    fs::write(&outside, "secret policy").unwrap();
    std::os::unix::fs::symlink(outside, source.join("link.md")).unwrap();
    assert!(!store.reconcile("kb").unwrap().failures.is_empty());
    assert!(store.search(query("secret")).unwrap().evidence.is_empty());
    fs::write(source.join("a.md"), [0xff]).unwrap();
    assert!(!store.reconcile("kb").unwrap().failures.is_empty());
    assert!(store.search(query("restored")).unwrap().evidence.is_empty());
    store.integrity().unwrap();
}
#[test]
fn backup_restores_and_second_writer_is_rejected() {
    let (temp, mut store) = setup(false);
    ingest(&mut store, "a.md", "durable policy");
    let error = Store::open(&temp.path().join("data"), None).err().unwrap();
    assert_eq!(error.code, "busy");
    let restore = temp.path().join("restore");
    fs::create_dir(&restore).unwrap();
    store.backup(&restore.join("store.sqlite")).unwrap();
    let mut restored = Store::open(&restore, None).unwrap();
    assert_eq!(restored.generation().unwrap(), store.generation().unwrap());
    assert!(
        !restored
            .search(query("durable"))
            .unwrap()
            .evidence
            .is_empty()
    );
    restored.integrity().unwrap();
}
#[test]
fn schema_roundtrip_and_envelope() {
    let (_temp, mut store) = setup(false);
    let request:Request=serde_json::from_str(r#"{"schemaVersion":1,"requestId":"test","operation":"search","query":"hello","collections":["kb"],"topK":3}"#).unwrap();
    let response = store.execute(request);
    assert!(response.error.is_none(), "{:?}", response.error);
    let request: Request =
        serde_json::from_str(r#"{"schemaVersion":99,"requestId":"test","operation":"status"}"#)
            .unwrap();
    assert_eq!(
        store.execute(request).error.unwrap().code,
        "unsupported_version"
    );
}
#[test]
fn nonfinite_and_zero_vectors_fail_closed() {
    assert!(know_things::embedding::encode(&[f32::NAN], 1).is_err());
    assert!(know_things::embedding::encode(&[0.], 1).is_err());
    assert!(know_things::embedding::encode(&[1.], 2).is_err());
}
#[test]
fn model_upgrade_activates_only_when_all_eligible_documents_are_covered() {
    let (temp, mut store) = setup(true);
    ingest(&mut store, "a.md", "policy a");
    ingest(&mut store, "b.md", "policy b");
    drop(store);
    let mut store = Store::open(
        &temp.path().join("data"),
        Some(Box::new(Fixture::new("replacement"))),
    )
    .unwrap();
    ingest(&mut store, "a.md", "policy a");
    assert_eq!(
        store.search(query("policy")).unwrap().actual_mode,
        Mode::Lexical
    );
    ingest(&mut store, "b.md", "policy b");
    assert_eq!(
        store.search(query("policy")).unwrap().actual_mode,
        Mode::Hybrid
    );
    store.integrity().unwrap();
}
#[test]
fn context_expansion_is_bounded_revision_pinned_and_resolvable() {
    let (_temp, mut store) = setup(false);
    ingest(
        &mut store,
        "guide.md",
        "# Cache\n\nRedis migration policy.\n\nAdditional guidance.\n\n# Other\n\nUnrelated section.\n",
    );
    let p = store.search(query("Redis")).unwrap();
    let reference = p
        .evidence
        .iter()
        .find(|e| e.text.contains("Redis"))
        .unwrap()
        .reference
        .clone();
    let expanded = store
        .expand_context(&reference, ContextScope::Section, 1000)
        .unwrap();
    assert!(expanded["text"].as_str().unwrap().contains("Additional"));
    assert!(!expanded["text"].as_str().unwrap().contains("Unrelated"));
    for e in expanded["evidence"].as_array().unwrap() {
        let r: Reference = serde_json::from_value(e["reference"].clone()).unwrap();
        assert_eq!(store.context(&r).unwrap(), e["text"]);
    }
    let small = store
        .expand_context(&reference, ContextScope::Document, 9)
        .unwrap();
    assert!(small["truncated"].as_bool().unwrap());
    assert!(small["tokenCount"].as_u64().unwrap() <= 9);
    ingest(&mut store, "guide.md", "entirely different current text");
    let old = store
        .expand_context(&reference, ContextScope::Section, 1000)
        .unwrap();
    assert_eq!(expanded["text"], old["text"]);
}
#[test]
fn retention_include_and_collection_removal_preserve_eligibility() {
    let (temp, mut store) = setup(false);
    let root = temp.path().join("source");
    fs::write(root.join("guide.md"), "old policy").unwrap();
    store.reconcile("kb").unwrap();
    let old = store.search(query("policy")).unwrap().evidence[0]
        .reference
        .clone();
    fs::write(root.join("guide.md"), "new policy").unwrap();
    store.reconcile("kb").unwrap();
    store.prune(1).unwrap();
    assert_eq!(
        store.context(&old).unwrap_err().code,
        "evidence_version_unavailable"
    );
    let current = store.search(query("policy")).unwrap().evidence[0]
        .reference
        .clone();
    store.exclude(&current.document_id).unwrap();
    store.reconcile("kb").unwrap();
    assert!(store.search(query("policy")).unwrap().evidence.is_empty());
    store.include(&current.document_id).unwrap();
    assert!(store.search(query("policy")).unwrap().evidence.is_empty());
    store.reconcile("kb").unwrap();
    assert!(!store.search(query("policy")).unwrap().evidence.is_empty());
    store.remove_collection("kb").unwrap();
    assert_eq!(
        store.context(&current).unwrap_err().code,
        "evidence_version_unavailable"
    );
    assert_eq!(
        fs::read_to_string(root.join("guide.md")).unwrap(),
        "new policy"
    );
    store.integrity().unwrap();
}
#[test]
fn restore_is_atomic_and_corrupt_canonical_content_is_detected() {
    let (temp, mut store) = setup(false);
    ingest(&mut store, "a.md", "durable source");
    let backup = temp.path().join("backup.sqlite");
    store.backup(&backup).unwrap();
    let dest = temp.path().join("restored");
    let restored = Store::restore(&backup, &dest).unwrap();
    restored.integrity().unwrap();
    assert!(Store::restore(&backup, &dest).is_err());
    drop(restored);
    drop(store);
    let db = rusqlite::Connection::open(&backup).unwrap();
    db.execute("UPDATE revisions SET raw=?1", [b"tampered".as_slice()])
        .unwrap();
    drop(db);
    let bad = temp.path().join("rejected");
    assert_eq!(
        Store::restore(&backup, &bad).err().unwrap().code,
        "store_corrupt"
    );
    assert!(!bad.exists());
}
#[test]
fn processing_upgrade_never_mixes_old_and_new_chunks_for_the_same_raw_revision() {
    let (temp, mut store) = setup(false);
    ingest(&mut store, "guide.md", "policy paragraph");
    let old = store.search(query("policy")).unwrap().evidence[0]
        .reference
        .clone();
    drop(store);
    let legacy = know_things::identity(&[
        "chunk",
        &old.revision_id,
        "legacy-parser",
        &old.byte_start.to_string(),
        &old.byte_end.to_string(),
    ]);
    let db = rusqlite::Connection::open(temp.path().join("data/store.sqlite")).unwrap();
    db.execute(
        "UPDATE chunks SET id=?1,processing='legacy-parser' WHERE id=?2",
        [&legacy, &old.chunk_id],
    )
    .unwrap();
    db.execute("UPDATE documents SET active_processing='legacy-parser'", [])
        .unwrap();
    drop(db);
    let mut reference = old.clone();
    reference.chunk_id = legacy;
    let mut store = Store::open(&temp.path().join("data"), None).unwrap();
    store.integrity().unwrap();
    assert_eq!(store.context(&reference).unwrap(), "policy paragraph");
    let staged = store
        .stage("kb", "guide.md", "policy paragraph")
        .unwrap()
        .unwrap();
    store.activate(staged).unwrap();
    let pack = store.search(query("policy")).unwrap();
    assert_eq!(pack.evidence.len(), 1);
    assert_eq!(pack.evidence[0].reference.chunk_id, old.chunk_id);
    assert_eq!(store.context(&reference).unwrap(), "policy paragraph");
    store.integrity().unwrap();
}
#[test]
fn section_expansion_does_not_merge_separate_repeated_heading_names() {
    let (_temp, mut store) = setup(false);
    ingest(
        &mut store,
        "guide.md",
        "# Repeated\n\nFirst occurrence unique-red.\n\n# Repeated\n\nSecond occurrence unique-blue.\n",
    );
    let pack = store.search(query("unique-red")).unwrap();
    let reference = pack
        .evidence
        .iter()
        .find(|e| e.text.contains("unique-red"))
        .unwrap()
        .reference
        .clone();
    let section = store
        .expand_context(&reference, ContextScope::Section, 1000)
        .unwrap();
    assert!(section["text"].as_str().unwrap().contains("unique-red"));
    assert!(!section["text"].as_str().unwrap().contains("unique-blue"));
}

#[test]
fn removing_and_recreating_a_collection_does_not_revalidate_an_old_job() {
    let (temp, mut store) = setup(false);
    let old = store
        .stage("kb", "guide.md", "Old policy.")
        .unwrap()
        .unwrap();
    store.remove_collection("kb").unwrap();
    store.register("kb", &temp.path().join("source")).unwrap();
    let replacement = store
        .stage("kb", "guide.md", "Replacement policy.")
        .unwrap()
        .unwrap();
    assert!(!store.activate(old).unwrap());
    assert!(store.activate(replacement).unwrap());
    let mut q = query("policy");
    q.mode = Mode::Lexical;
    let pack = store.search(q).unwrap();
    assert_eq!(pack.evidence.len(), 1);
    assert_eq!(pack.evidence[0].text, "Replacement policy.");
    store.integrity().unwrap();
}

#[test]
fn repeating_a_lexical_query_word_does_not_implicitly_boost_its_weight() {
    let (_temp, mut store) = setup(false);
    ingest(&mut store, "redis.md", "Redis guide.");
    ingest(&mut store, "cache.md", "Cache guide.");
    let mut base = query("redis cache");
    base.mode = Mode::Lexical;
    let expected = store.search(base.clone()).unwrap();
    for text in [
        "redis redis cache",
        "redis cache cache CACHE",
        "REDIS cache redis",
    ] {
        let mut repeated = base.clone();
        repeated.query = text.into();
        let actual = store.search(repeated).unwrap();
        assert_eq!(actual.evidence_pack_id, expected.evidence_pack_id);
    }
}

#[test]
fn backup_publication_never_overwrites_a_destination_and_cleans_staging() {
    let (temp, mut store) = setup(false);
    ingest(&mut store, "guide.md", "backup policy");
    let destination = temp.path().join("backup.sqlite");
    fs::write(&destination, "existing user file").unwrap();
    assert!(store.backup(&destination).is_err());
    assert_eq!(
        fs::read_to_string(&destination).unwrap(),
        "existing user file"
    );
    assert!(!fs::read_dir(temp.path()).unwrap().any(|p| {
        p.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".backup-")
    }));
    fs::remove_file(&destination).unwrap();
    store.backup(&destination).unwrap();
    let restored = Store::restore(&destination, &temp.path().join("restore")).unwrap();
    restored.integrity().unwrap();
}

#[test]
fn empty_sqlite_files_cannot_be_mistaken_for_valid_backups() {
    let temp = tempfile::tempdir().unwrap();
    let backup = temp.path().join("empty.sqlite");
    drop(rusqlite::Connection::open(&backup).unwrap());
    let destination = temp.path().join("restore");
    assert_eq!(
        Store::restore(&backup, &destination).err().unwrap().code,
        "store_incompatible"
    );
    assert!(!destination.exists());
}

#[test]
fn a_schema_one_backup_is_migrated_without_changing_the_backup() {
    let temp = tempfile::tempdir().unwrap();
    let backup = temp.path().join("v1.sqlite");
    let source = rusqlite::Connection::open(&backup).unwrap();
    source
        .execute_batch(include_str!("../migrations/001.sql"))
        .unwrap();
    drop(source);
    let original_hash = know_things::hash(&fs::read(&backup).unwrap());
    let destination = temp.path().join("restored");
    let restored = Store::restore(&backup, &destination).unwrap();
    restored.integrity().unwrap();
    assert_eq!(restored.generation().unwrap(), 0);
    drop(restored);
    let upgraded = rusqlite::Connection::open_with_flags(
        destination.join("store.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        upgraded
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        6
    );
    assert_eq!(
        know_things::hash(&fs::read(&backup).unwrap()),
        original_hash
    );
}

#[test]
fn schema_two_upgrade_preserves_all_canonical_rows_and_dense_results() {
    let (temp, mut store) = setup(true);
    ingest(&mut store, "guide.md", "automobile policy");
    let mut request = query("car");
    request.strict_hybrid = true;
    let before = store.search(request.clone()).unwrap();
    assert_eq!(before.actual_mode, Mode::Hybrid);
    assert!(!before.evidence.is_empty());
    let generation = store.generation().unwrap();
    drop(store);
    let db = rusqlite::Connection::open(temp.path().join("data/store.sqlite")).unwrap();
    // Schema 2 had no policy rows; its subsequent migration creates the legacy
    // default JSON, with newly introduced optional settings absent.
    db.execute("UPDATE collection_policies SET policy='{\"excludedPaths\":[],\"reconcileIntervalMs\":null}'",[]).unwrap();
    let rows = canonical_rows(&db);
    support::legacy_vectors(&db);
    db.execute_batch("DROP TABLE collection_policies; PRAGMA user_version=2;")
        .unwrap();
    drop(db);
    let mut store =
        Store::open(&temp.path().join("data"), Some(Box::new(Fixture::new("a")))).unwrap();
    store.integrity().unwrap();
    assert_eq!(store.generation().unwrap(), generation);
    let after = store.search(request).unwrap();
    assert_eq!(after.actual_mode, Mode::Hybrid);
    assert_eq!(after.evidence[0].reference, before.evidence[0].reference);
    assert_eq!(
        store.context(&before.evidence[0].reference).unwrap(),
        "automobile policy"
    );
    let db = rusqlite::Connection::open(temp.path().join("data/store.sqlite")).unwrap();
    assert_eq!(canonical_rows(&db), rows);
}

#[test]
fn pruning_batches_preserve_current_and_recent_revisions_and_remove_obsolete_vectors() {
    let (_temp, mut store) = setup(true);
    let mut references = Vec::new();
    for n in 0..6 {
        ingest(&mut store, "guide.md", &format!("policy revision {n}"));
        references.push(
            store.search(query("policy")).unwrap().evidence[0]
                .reference
                .clone(),
        );
    }
    for remaining in [2, 0] {
        let response = store.execute(
            serde_json::from_value(serde_json::json!({
                "schemaVersion":1,"requestId":"batch","operation":"prune",
                "retainRevisions":2,"maxRevisions":2,"deadlineMs":5000
            }))
            .unwrap(),
        );
        assert_eq!(response.status, "ok");
        let report = response.data.unwrap();
        assert_eq!(report["revisionsRemoved"], 2);
        assert_eq!(report["revisionsRemaining"], remaining);
        assert_eq!(report["moreRemaining"], remaining > 0);
        assert_eq!(report["leasedRevisionsDeferred"], 0);
        store.integrity().unwrap();
        assert_eq!(store.context(&references[5]).unwrap(), "policy revision 5");
        assert_eq!(store.context(&references[4]).unwrap(), "policy revision 4");
    }
    for reference in &references[..4] {
        assert_eq!(
            store.context(reference).unwrap_err().code,
            "evidence_version_unavailable"
        );
    }
    assert_eq!(
        store.search(query("policy")).unwrap().actual_mode,
        Mode::Hybrid
    );
}

fn canonical_rows(connection: &rusqlite::Connection) -> Vec<Vec<rusqlite::types::Value>> {
    let mut output = Vec::new();
    for table in [
        "meta",
        "collections",
        "collection_policies",
        "documents",
        "revisions",
        "chunks",
        "models",
        "vectors",
        "vector_values",
        "jobs",
    ] {
        let condition = if table == "meta" {
            " WHERE key!='generation'"
        } else {
            ""
        };
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {table}{condition} ORDER BY rowid"))
            .unwrap();
        let columns = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|i| row.get(i))
                    .collect::<rusqlite::Result<Vec<rusqlite::types::Value>>>()
            })
            .unwrap();
        output.extend(rows.map(std::result::Result::unwrap));
    }
    output
}

#[test]
fn rebuilding_fts_repairs_derived_damage_without_reviving_sources_or_rewriting_canonical_data() {
    for damage in ["missing", "content", "identifiers", "postings"] {
        let (temp, mut store) = setup(true);
        ingest(&mut store, "guide.md", "oldpolicy redis.example");
        let mut q = query("oldpolicy");
        q.mode = Mode::Lexical;
        let old = store.search(q).unwrap().evidence.remove(0);
        ingest(&mut store, "guide.md", "newpolicy redis.example");
        ingest(&mut store, "excluded.md", "secretpolicy hidden.example");
        let hidden = store
            .search(Search {
                mode: Mode::Lexical,
                ..query("secretpolicy")
            })
            .unwrap()
            .evidence
            .remove(0);
        store.exclude(&hidden.reference.document_id).unwrap();
        let generation = store.generation().unwrap();
        drop(store);
        fs::rename(
            temp.path().join("source"),
            temp.path().join("source-unavailable"),
        )
        .unwrap();
        let connection = rusqlite::Connection::open(temp.path().join("data/store.sqlite")).unwrap();
        let before = canonical_rows(&connection);
        match damage {
            "missing" => connection.execute_batch("DROP TABLE lexical").unwrap(),
            "content" => {
                connection
                    .execute(
                        "UPDATE lexical SET text='incorrect derived text',title='wrong title'",
                        [],
                    )
                    .unwrap();
            }
            "identifiers" => {
                connection
                    .execute("UPDATE lexical SET identifiers='rogue.identifier'", [])
                    .unwrap();
            }
            _ => {
                assert!(
                    connection
                        .execute("DELETE FROM lexical_data WHERE id>10", [])
                        .unwrap()
                        > 0
                );
            }
        }
        drop(connection);
        let mut store = Store::open(&temp.path().join("data"), None).unwrap();
        assert!(store.integrity().is_err(), "must detect {damage}");
        let result = store.rebuild_lexical(5000).unwrap();
        assert_eq!(result["sourceFilesRead"], false);
        assert_eq!(store.generation().unwrap(), generation + 1);
        store.integrity().unwrap();
        assert_eq!(store.context(&old.reference).unwrap(), old.text);
        assert_eq!(
            store.context(&hidden.reference).unwrap_err().code,
            "source_unavailable"
        );
        for text in ["oldpolicy", "secretpolicy", "rogue.identifier"] {
            assert!(
                store
                    .search(Search {
                        mode: Mode::Lexical,
                        ..query(text)
                    })
                    .unwrap()
                    .evidence
                    .is_empty()
            );
        }
        assert_eq!(
            store
                .search(Search {
                    mode: Mode::Lexical,
                    ..query("newpolicy")
                })
                .unwrap()
                .evidence[0]
                .text,
            "newpolicy redis.example"
        );
        drop(store);
        let connection = rusqlite::Connection::open(temp.path().join("data/store.sqlite")).unwrap();
        assert_eq!(
            canonical_rows(&connection),
            before,
            "canonical bytes changed for {damage}"
        );
    }
}

#[test]
fn lexical_recovery_refuses_corrupt_canonical_bytes_vectors_or_active_revisions() {
    for damage in [
        "raw",
        "vectors",
        "active_revision",
        "document_scope",
        "collection_scope",
        "eligibility",
    ] {
        let (temp, mut store) = setup(true);
        ingest(&mut store, "guide.md", "policy redis.example");
        ingest(&mut store, "other.md", "other policy");
        let generation = store.generation().unwrap();
        drop(store);
        let connection = rusqlite::Connection::open(temp.path().join("data/store.sqlite")).unwrap();
        match damage {
            "raw" => {
                connection.execute("UPDATE revisions SET raw=?1 WHERE rowid=(SELECT min(rowid) FROM revisions)", [b"tampered".as_slice()]).unwrap();
            }
            "vectors" => {
                connection
                    .execute("UPDATE vector_values SET data=x'00000000'", [])
                    .unwrap();
            }
            "active_revision" => {
                connection.execute("UPDATE documents SET active_revision=(SELECT active_revision FROM documents WHERE path='other.md') WHERE path='guide.md'", []).unwrap();
            }
            "document_scope" => {
                connection
                    .execute(
                        "UPDATE documents SET path='relocated.md' WHERE path='guide.md'",
                        [],
                    )
                    .unwrap();
            }
            "collection_scope" => {
                connection
                    .execute("UPDATE collections SET root=root||'/other'", [])
                    .unwrap();
            }
            _ => {
                connection
                    .execute(
                        "UPDATE documents SET excluded=1,eligible=1 WHERE path='guide.md'",
                        [],
                    )
                    .unwrap();
            }
        }
        let index_rows = |connection: &rusqlite::Connection| {
            connection
                .prepare("SELECT rowid,text,title,heading,identifiers FROM lexical ORDER BY rowid")
                .unwrap()
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let before = index_rows(&connection);
        drop(connection);
        let mut store = Store::open(&temp.path().join("data"), None).unwrap();
        assert_eq!(
            store.rebuild_lexical(5000).unwrap_err().code,
            "store_corrupt"
        );
        assert_eq!(store.generation().unwrap(), generation);
        drop(store);
        let connection = rusqlite::Connection::open(temp.path().join("data/store.sqlite")).unwrap();
        assert_eq!(index_rows(&connection), before);
    }
}

#[test]
fn restore_rebuilds_a_missing_derived_index_and_preserves_backup_bytes_and_generation() {
    let (temp, mut store) = setup(true);
    ingest(&mut store, "guide.md", "restorable redis.example");
    let reference = store
        .search(Search {
            mode: Mode::Lexical,
            ..query("restorable")
        })
        .unwrap()
        .evidence
        .remove(0);
    let generation = store.generation().unwrap();
    let backup = temp.path().join("backup.sqlite");
    store.backup(&backup).unwrap();
    let connection = rusqlite::Connection::open(&backup).unwrap();
    connection.execute_batch("DROP TABLE lexical").unwrap();
    drop(connection);
    let before = know_things::hash(&fs::read(&backup).unwrap());
    fs::rename(
        temp.path().join("source"),
        temp.path().join("source-unavailable"),
    )
    .unwrap();
    let mut restored = Store::restore(&backup, &temp.path().join("restore")).unwrap();
    restored.integrity().unwrap();
    assert_eq!(restored.generation().unwrap(), generation);
    assert_eq!(
        restored.context(&reference.reference).unwrap(),
        reference.text
    );
    assert_eq!(
        restored
            .search(Search {
                mode: Mode::Lexical,
                ..query("restorable")
            })
            .unwrap()
            .evidence[0]
            .reference,
        reference.reference
    );
    assert_eq!(know_things::hash(&fs::read(&backup).unwrap()), before);
}
