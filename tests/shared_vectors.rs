use know_things::{
    contracts::*,
    embedding::{Embedder, Manifest},
    store::Store,
};
use std::{
    collections::BTreeMap,
    fs,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
mod support;

struct Fixture {
    manifest: Manifest,
    calls: Arc<AtomicUsize>,
}
impl Fixture {
    fn new(name: &str, calls: Arc<AtomicUsize>) -> Self {
        Self {
            calls,
            manifest: Manifest {
                name: name.into(),
                artifacts: BTreeMap::new(),
                tokenizer: "fixture".into(),
                architecture: "fixture-not-semantic".into(),
                quantization: "f32".into(),
                runtime: "fixture".into(),
                query_prefix: "query".into(),
                document_prefix: "document".into(),
                pooling: "fixture".into(),
                normalization: "l2".into(),
                dimensions: 2,
                max_input_tokens: 100,
                license: "fixture".into(),
            },
        }
    }
}
impl Embedder for Fixture {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    fn embed(&mut self, _: &str, query: bool, _: Instant) -> Result<Vec<f32>> {
        if !query {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
        Ok(vec![1., 0.])
    }
}
fn query(collection: &str) -> Search {
    Search {
        query: "policy".into(),
        collections: vec![collection.into()],
        top_k: 20,
        evidence_token_budget: 12000,
        deadline_ms: 5000,
        mode: Mode::Hybrid,
        strict_hybrid: true,
    }
}
fn setup() -> (tempfile::TempDir, Store, Arc<AtomicUsize>) {
    let temporary = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut store = Store::open(
        &temporary.path().join("data"),
        Some(Box::new(Fixture::new("a", calls.clone()))),
    )
    .unwrap();
    for name in ["a", "b"] {
        fs::create_dir(temporary.path().join(name)).unwrap();
        store.register(name, &temporary.path().join(name)).unwrap();
    }
    (temporary, store, calls)
}
fn ingest(store: &mut Store, collection: &str, path: &str, raw: &str) {
    let staged = store.stage(collection, path, raw).unwrap().unwrap();
    assert!(store.activate(staged).unwrap());
}
fn counts(temporary: &tempfile::TempDir) -> (i64, i64) {
    let connection =
        rusqlite::Connection::open(temporary.path().join("data/store.sqlite")).unwrap();
    connection
        .query_row(
            "SELECT (SELECT count(*) FROM vectors),(SELECT count(*) FROM vector_values)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

#[test]
fn identical_inputs_share_inference_and_storage_without_merging_source_eligibility() {
    let (temporary, mut store, calls) = setup();
    ingest(&mut store, "a", "first.md", "shared policy\n\n");
    ingest(&mut store, "b", "second.md", "shared policy\n\n");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(counts(&temporary), (2, 1));
    let a = store.search(query("a")).unwrap().evidence.remove(0);
    let b = store.search(query("b")).unwrap().evidence.remove(0);
    assert_ne!(a.reference.document_id, b.reference.document_id);
    assert_ne!(a.reference.chunk_id, b.reference.chunk_id);
    store.exclude(&a.reference.document_id).unwrap();
    assert!(store.search(query("a")).unwrap().evidence.is_empty());
    assert_eq!(
        store.context(&a.reference).unwrap_err().code,
        "source_unavailable"
    );
    assert_eq!(store.context(&b.reference).unwrap(), b.text);
    store.remove_collection("a").unwrap();
    assert_eq!(counts(&temporary), (1, 1));
    assert_eq!(store.context(&b.reference).unwrap(), b.text);
    store.integrity().unwrap();
    store.remove_collection("b").unwrap();
    assert_eq!(counts(&temporary), (0, 0));
    store.integrity().unwrap();
}

#[test]
fn repeated_chunks_reuse_locally_and_persisted_values_survive_restart_and_reclaim_after_prune() {
    let (temporary, mut store, calls) = setup();
    ingest(
        &mut store,
        "a",
        "first.md",
        "shared policy\n\nshared policy\n\n",
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(counts(&temporary), (2, 1));
    let old = store.search(query("a")).unwrap().evidence.remove(0);
    drop(store);
    let mut store = Store::open(
        &temporary.path().join("data"),
        Some(Box::new(Fixture::new("a", calls.clone()))),
    )
    .unwrap();
    ingest(&mut store, "b", "copy.md", "shared policy\n\n");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    ingest(&mut store, "a", "first.md", "new policy\n\n");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(store.context(&old.reference).unwrap(), old.text);
    store.prune(1).unwrap();
    assert_eq!(counts(&temporary), (2, 2));
    assert_eq!(
        store.context(&old.reference).unwrap_err().code,
        "evidence_version_unavailable"
    );
    store.remove_collection("b").unwrap();
    assert_eq!(counts(&temporary), (1, 1));
    store.integrity().unwrap();
}

#[test]
fn reuse_never_crosses_embedding_spaces_and_backup_restore_preserves_shared_owners() {
    let (temporary, mut store, calls) = setup();
    ingest(&mut store, "a", "first.md", "shared policy");
    ingest(&mut store, "b", "second.md", "shared policy");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    store
        .attach_embedder(Box::new(Fixture::new("replacement", calls.clone())))
        .unwrap();
    ingest(&mut store, "a", "first.md", "shared policy");
    assert_eq!(
        store.search(query("a")).unwrap_err().code,
        "model_unavailable"
    );
    ingest(&mut store, "b", "second.md", "shared policy");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(counts(&temporary), (4, 2));
    let evidence = store.search(query("b")).unwrap().evidence.remove(0);
    let backup = temporary.path().join("backup.sqlite");
    store.backup(&backup).unwrap();
    let restored = Store::restore(&backup, &temporary.path().join("restored")).unwrap();
    assert_eq!(
        restored.context(&evidence.reference).unwrap(),
        evidence.text
    );
    restored.integrity().unwrap();
    let connection =
        rusqlite::Connection::open(temporary.path().join("restored/store.sqlite")).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM vector_values", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn schema_four_migration_preserves_distinct_legacy_results_for_identical_inputs() {
    let (temporary, mut store, calls) = setup();
    ingest(&mut store, "a", "first.md", "shared policy");
    ingest(&mut store, "b", "second.md", "shared policy");
    let evidence = store.search(query("a")).unwrap().evidence.remove(0);
    let generation = store.generation().unwrap();
    drop(store);
    let connection =
        rusqlite::Connection::open(temporary.path().join("data/store.sqlite")).unwrap();
    support::legacy_vectors(&connection);
    let differing = [0f32.to_le_bytes(), 1f32.to_le_bytes()].concat();
    connection
        .execute(
            "UPDATE vectors SET data=?1 WHERE chunk_id=?2",
            rusqlite::params![differing, evidence.reference.chunk_id],
        )
        .unwrap();
    let old: Vec<(String, Vec<u8>)> = connection
        .prepare("SELECT chunk_id,data FROM vectors ORDER BY chunk_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(std::result::Result::unwrap)
        .collect();
    drop(connection);
    let upgraded = Store::open(&temporary.path().join("data"), None).unwrap();
    assert_eq!(upgraded.generation().unwrap(), generation);
    assert_eq!(
        upgraded.context(&evidence.reference).unwrap(),
        evidence.text
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(counts(&temporary), (2, 2));
    upgraded.integrity().unwrap();
    let connection =
        rusqlite::Connection::open(temporary.path().join("data/store.sqlite")).unwrap();
    let new:Vec<(String,Vec<u8>)>=connection.prepare("SELECT v.chunk_id,e.data FROM vectors v JOIN vector_values e ON e.id=v.value_id ORDER BY v.chunk_id").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?))).unwrap().map(std::result::Result::unwrap).collect();
    assert_eq!(old, new);
}

#[test]
fn corrupted_shared_values_are_not_reused_or_repaired_silently() {
    let (temporary, mut store, calls) = setup();
    ingest(&mut store, "a", "first.md", "shared policy");
    let generation = store.generation().unwrap();
    let connection =
        rusqlite::Connection::open(temporary.path().join("data/store.sqlite")).unwrap();
    connection
        .execute("UPDATE vector_values SET data=zeroblob(8)", [])
        .unwrap();
    let error = store.stage("b", "second.md", "shared policy").unwrap_err();
    assert_eq!(error.code, "store_corrupt");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.generation().unwrap(), generation);
    assert_eq!(counts(&temporary), (1, 1));
    assert_eq!(store.integrity().unwrap_err().code, "store_corrupt");
}

#[test]
fn cached_staging_remains_valid_when_another_sources_last_owner_is_removed() {
    let (temporary, mut store, calls) = setup();
    ingest(&mut store, "a", "first.md", "shared policy");
    let staged = store
        .stage("b", "second.md", "shared policy")
        .unwrap()
        .unwrap();
    store.remove_collection("a").unwrap();
    assert_eq!(counts(&temporary), (0, 0));
    assert!(store.activate(staged).unwrap());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(counts(&temporary), (1, 1));
    assert_eq!(
        store.search(query("b")).unwrap().evidence[0].text,
        "shared policy"
    );
    store.integrity().unwrap();
}

#[test]
fn failed_publication_rolls_back_new_shared_values_and_their_mappings() {
    let (temporary, mut store, _) = setup();
    ingest(&mut store, "a", "first.md", "shared policy");
    let generation = store.generation().unwrap();
    let connection =
        rusqlite::Connection::open(temporary.path().join("data/store.sqlite")).unwrap();
    connection
        .execute(
            "UPDATE meta SET value='9223372036854775807' WHERE key='generation'",
            [],
        )
        .unwrap();
    let staged = store
        .stage("b", "second.md", "different policy")
        .unwrap()
        .unwrap();
    assert_eq!(
        store.activate(staged).unwrap_err().code,
        "resource_exhausted"
    );
    assert_eq!(counts(&temporary), (1, 1));
    connection
        .execute(
            "UPDATE meta SET value=?1 WHERE key='generation'",
            [generation.to_string()],
        )
        .unwrap();
    store.integrity().unwrap();
    assert!(store.search(query("b")).unwrap().evidence.is_empty());
}

#[test]
fn invalid_legacy_vectors_refuse_upgrade_without_changing_schema_or_bytes() {
    let (temporary, mut store, _) = setup();
    ingest(&mut store, "a", "first.md", "shared policy");
    drop(store);
    let connection =
        rusqlite::Connection::open(temporary.path().join("data/store.sqlite")).unwrap();
    support::legacy_vectors(&connection);
    connection
        .execute("UPDATE vectors SET data=zeroblob(8)", [])
        .unwrap();
    drop(connection);
    match Store::open(&temporary.path().join("data"), None) {
        Err(error) => assert_eq!(error.code, "store_corrupt"),
        Ok(_) => panic!("Corrupt legacy vectors were accepted"),
    }
    let connection =
        rusqlite::Connection::open(temporary.path().join("data/store.sqlite")).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert_eq!(
        connection
            .query_row("SELECT data FROM vectors", [], |r| r.get::<_, Vec<u8>>(0))
            .unwrap(),
        vec![0; 8]
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='vector_values'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}
