//! Scheduled cleanup uses disposable stores and never needs source/model access.
use know_things::{contracts::*, coordinator::Coordinator, store::Store};
use serde_json::{Value, json};
use std::{
    fs,
    time::{Duration, Instant},
};

fn call(c: &Coordinator, mut value: Value) -> Response {
    value["schemaVersion"] = json!(1);
    value["requestId"] = json!("retention");
    c.execute(serde_json::from_value(value).unwrap())
}
fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(6);
    while !ready() {
        assert!(Instant::now() < deadline, "bounded retention observation");
        std::thread::sleep(Duration::from_millis(15));
    }
}
fn settings(retain: usize) -> CollectionPolicy {
    CollectionPolicy {
        retention: Some(RetentionPolicy {
            retain_revisions: retain,
            interval_ms: 1000,
            max_revisions: 1,
            deadline_ms: 1000,
        }),
        ..CollectionPolicy::default()
    }
}
fn history(store: &mut Store, collection: &str) -> Vec<Reference> {
    (0..5)
        .map(|n| {
            let staged = store
                .stage(collection, "guide.md", &format!("policy revision {n}"))
                .unwrap()
                .unwrap();
            assert!(store.activate(staged).unwrap());
            store
                .search(Search {
                    query: "policy".into(),
                    collections: vec![collection.into()],
                    top_k: 20,
                    evidence_token_budget: 12000,
                    deadline_ms: 5000,
                    mode: Mode::Lexical,
                    strict_hybrid: false,
                })
                .unwrap()
                .evidence[0]
                .reference
                .clone()
        })
        .collect()
}
fn setup() -> (tempfile::TempDir, Store) {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("source")).unwrap();
    let mut store = Store::open(&temp.path().join("data"), None).unwrap();
    store.register("kb", &temp.path().join("source")).unwrap();
    (temp, store)
}

#[test]
fn retention_settings_validate_atomically_and_survive_backup_restore() {
    let (temp, mut store) = setup();
    let generation = store.generation().unwrap();
    let valid = settings(2);
    for (field, value) in [
        ("retainRevisions", 0),
        ("retainRevisions", 1001),
        ("maxRevisions", 0),
        ("maxRevisions", 1001),
        ("intervalMs", 999),
        ("intervalMs", 86400001),
        ("deadlineMs", 0),
        ("deadlineMs", 5001),
    ] {
        let mut invalid = serde_json::to_value(&valid).unwrap();
        invalid["retention"][field] = json!(value);
        let invalid: CollectionPolicy = serde_json::from_value(invalid).unwrap();
        assert_eq!(
            store.set_collection_policy("kb", invalid).unwrap_err().code,
            "invalid_request"
        );
        assert_eq!(store.generation().unwrap(), generation);
        assert_eq!(
            store.collection_policy("kb").unwrap(),
            CollectionPolicy::default()
        );
    }
    store.set_collection_policy("kb", valid.clone()).unwrap();
    store.register("kb", &temp.path().join("source")).unwrap();
    assert_eq!(store.collection_policy("kb").unwrap(), valid);
    let backup = temp.path().join("backup.sqlite");
    store.backup(&backup).unwrap();
    let hash = know_things::hash(&fs::read(&backup).unwrap());
    let restored = Store::restore(&backup, &temp.path().join("restored")).unwrap();
    assert_eq!(restored.collection_policy("kb").unwrap(), valid);
    assert_eq!(know_things::hash(&fs::read(&backup).unwrap()), hash);
    restored.integrity().unwrap();
}

#[test]
fn scheduled_retention_is_scoped_bounded_persisted_and_independent_of_source_availability() {
    let (temp, mut store) = setup();
    let id = store.register("kb", &temp.path().join("source")).unwrap();
    let kb = history(&mut store, "kb");
    fs::create_dir(temp.path().join("other")).unwrap();
    store.register("other", &temp.path().join("other")).unwrap();
    let other = history(&mut store, "other");
    store.set_collection_policy("kb", settings(1)).unwrap();
    drop(store);
    fs::remove_dir(temp.path().join("source")).unwrap();
    // Settings written by the previous coordinator survive reopening.
    let c = Coordinator::new(Store::open(&temp.path().join("data"), None).unwrap(), None);
    wait_for(|| {
        call(&c, json!({"operation":"status"})).data.unwrap()["periodicReconciliation"]["retention"]
            [&id]["completedPasses"]
            .as_u64()
            .unwrap_or(0)
            > 0
    });
    let status = call(&c, json!({"operation":"status"})).data.unwrap();
    assert_eq!(status["periodicReconciliation"]["configuredCollections"], 0);
    assert_eq!(
        status["periodicReconciliation"]["configuredRetentionCollections"],
        1
    );
    assert_eq!(
        status["periodicReconciliation"]["retention"][&id]["lastReport"]["revisionsRemoved"],
        1
    );
    assert_eq!(
        call(
            &c,
            json!({"operation":"set_collection_policy","collection":"kb","policy":{}})
        )
        .status,
        "ok"
    );
    wait_for(|| {
        call(&c, json!({"operation":"status"})).data.unwrap()["periodicReconciliation"]["configuredRetentionCollections"]
            == 0
    });
    let availability = || {
        kb.iter()
            .map(|reference| call(&c, json!({"operation":"context","reference":reference})).status)
            .collect::<Vec<_>>()
    };
    let before = availability();
    assert!(before.iter().any(|status| status == "error"));
    assert_eq!(before.last().unwrap(), "ok");
    for reference in other {
        assert_eq!(
            call(&c, json!({"operation":"context","reference":reference})).status,
            "ok"
        );
    }
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(
        availability(),
        before,
        "disabled maintenance must stop removing history"
    );
    assert_eq!(call(&c, json!({"operation":"integrity"})).status, "ok");
    let stopped = Instant::now();
    drop(c);
    assert!(stopped.elapsed() < Duration::from_secs(2));
    Store::open(&temp.path().join("data"), None)
        .unwrap()
        .integrity()
        .unwrap();
}

#[test]
fn empty_automatic_passes_do_not_advance_corpus_generation() {
    let (temp, mut store) = setup();
    let id = store.register("kb", &temp.path().join("source")).unwrap();
    history(&mut store, "kb");
    store.set_collection_policy("kb", settings(100)).unwrap();
    let generation = store.generation().unwrap();
    let c = Coordinator::new(store, None);
    wait_for(|| {
        call(&c, json!({"operation":"status"})).data.unwrap()["periodicReconciliation"]["retention"]
            [&id]["completedPasses"]
            .as_u64()
            .unwrap_or(0)
            >= 2
    });
    let status = call(&c, json!({"operation":"status"}));
    assert_eq!(status.corpus_generation, Some(generation));
    assert_eq!(
        status.data.unwrap()["periodicReconciliation"]["retention"][&id]["lastReport"]["revisionsRemoved"],
        0
    );
}
