#![cfg(target_os = "macos")]
use know_things::{contracts::*, coordinator::Coordinator, store::Store};
use serde_json::{Value, json};
use std::{
    fs,
    time::{Duration, Instant},
};

fn call(coordinator: &Coordinator, mut request: Value) -> Value {
    request["schemaVersion"] = json!(1);
    request["requestId"] = json!("watch-fixture");
    let response = coordinator.execute(serde_json::from_value(request).unwrap());
    assert_eq!(response.status, "ok", "{response:?}");
    response.data.unwrap()
}
fn search(c: &Coordinator, query: &str) -> Value {
    call(
        c,
        json!({"operation":"search","query":query,"collections":["fixture"],"mode":"lexical"}),
    )["evidence"]
        .clone()
}
fn status(c: &Coordinator) -> Value {
    call(c, json!({"operation":"status"}))["periodicReconciliation"].clone()
}
fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "watch observation deadline");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn policy() -> Value {
    json!({"watch":true,"reconcileIntervalMs":86400000,"excludedPaths":["private"]})
}
fn setup() -> (tempfile::TempDir, Coordinator, String) {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("guide.md"), "original policy\n").unwrap();
    fs::create_dir(source.join("private")).unwrap();
    fs::write(source.join("private/invalid.txt"), [255]).unwrap();
    let store = Store::open(&temporary.path().join("data"), None).unwrap();
    let c = Coordinator::new(store, None);
    let id = call(
        &c,
        json!({"operation":"register","name":"fixture","root":source,"policy":policy()}),
    )["collectionId"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for(|| {
        status(&c)["watch"]["collections"][&id]["active"] == true
            && status(&c)["collections"][&id]["completedScans"]
                .as_u64()
                .unwrap_or(0)
                > 0
    });
    (temporary, c, id)
}

#[test]
fn native_hints_drive_edits_atomic_replacement_nested_creation_rename_and_delete() {
    let (temporary, c, id) = setup();
    let source = temporary.path().join("source");
    let old: Reference =
        serde_json::from_value(search(&c, "original")[0]["reference"].clone()).unwrap();
    let initial_events = status(&c)["watch"]["observedEvents"].as_u64().unwrap();
    fs::write(source.join("guide.md"), "edited policy\n").unwrap();
    wait_for(|| !search(&c, "edited").as_array().unwrap().is_empty());
    assert_eq!(
        call(&c, json!({"operation":"context","reference":old}))["text"],
        "original policy\n"
    );
    fs::write(source.join("replacement.tmp"), "atomic policy\n").unwrap();
    fs::rename(source.join("replacement.tmp"), source.join("guide.md")).unwrap();
    wait_for(|| !search(&c, "atomic").as_array().unwrap().is_empty());
    fs::create_dir(source.join("nested")).unwrap();
    fs::write(source.join("nested/new.md"), "nestedaddition policy\n").unwrap();
    wait_for(|| !search(&c, "nestedaddition").as_array().unwrap().is_empty());
    wait_for(|| status(&c)["watch"]["collections"][&id]["watchedTargets"] == 4);
    fs::rename(source.join("guide.md"), source.join("renamed.md")).unwrap();
    wait_for(|| {
        let rows = search(&c, "atomic");
        let rows = rows.as_array().unwrap();
        rows.iter().any(|e| e["source"] == "renamed.md")
            && rows.iter().all(|e| e["source"] != "guide.md")
    });
    assert!(
        search(&c, "atomic")
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["source"] != "guide.md")
    );
    fs::remove_file(source.join("renamed.md")).unwrap();
    wait_for(|| search(&c, "atomic").as_array().unwrap().is_empty());
    assert!(status(&c)["watch"]["observedEvents"].as_u64().unwrap() > initial_events);
    assert_eq!(status(&c)["watch"]["safetyScanMaxIntervalMs"], 60000);
    call(&c, json!({"operation":"integrity"}));
}

#[test]
fn exclusions_symlinks_and_disable_preserve_the_shared_ingestion_boundary() {
    let (temporary, c, id) = setup();
    let source = temporary.path().join("source");
    assert_eq!(status(&c)["watch"]["collections"][&id]["watchedTargets"], 2);
    assert_eq!(status(&c)["collections"][&id]["lastReport"]["failures"], 0);
    let outside = temporary.path().join("outside.md");
    fs::write(&outside, "foreignsecret\n").unwrap();
    std::os::unix::fs::symlink(&outside, source.join("linked.md")).unwrap();
    wait_for(|| status(&c)["collections"][&id]["lastReport"]["failures"] == 1);
    assert!(search(&c, "foreignsecret").as_array().unwrap().is_empty());
    assert_eq!(status(&c)["watch"]["collections"][&id]["watchedTargets"], 2);
    call(
        &c,
        json!({"operation":"set_collection_policy","collection":"fixture","policy":{"excludedPaths":["private"]}}),
    );
    wait_for(|| {
        status(&c)["configuredCollections"] == 0
            && status(&c)["watch"]["collections"]
                .as_object()
                .is_some_and(|v| v.is_empty())
    });
    fs::write(source.join("guide.md"), "disabledchange\n").unwrap();
    std::thread::sleep(Duration::from_millis(700));
    assert!(search(&c, "disabledchange").as_array().unwrap().is_empty());
    call(&c, json!({"operation":"reconcile","collection":"fixture"}));
    assert!(!search(&c, "disabledchange").as_array().unwrap().is_empty());
    call(&c, json!({"operation":"integrity"}));
}

#[test]
fn persisted_watches_reconcile_offline_edits_and_shutdown_releases_store_ownership() {
    let (temporary, c, _) = setup();
    let source = temporary.path().join("source");
    let started = Instant::now();
    drop(c);
    assert!(started.elapsed() < Duration::from_secs(2));
    fs::write(source.join("guide.md"), "offlinechange\n").unwrap();
    let store = Store::open(&temporary.path().join("data"), None).unwrap();
    assert!(store.collection_policy("fixture").unwrap().watch);
    let c = Coordinator::new(store, None);
    wait_for(|| !search(&c, "offlinechange").as_array().unwrap().is_empty());
    drop(c);
    Store::open(&temporary.path().join("data"), None)
        .unwrap()
        .integrity()
        .unwrap();
}

#[test]
fn schema_five_upgrade_defaults_watching_off_and_preserves_references_and_generation() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("guide.md"), "durable policy\n").unwrap();
    let mut store = Store::open(&temporary.path().join("data"), None).unwrap();
    store.register("fixture", &source).unwrap();
    store.reconcile("fixture").unwrap();
    let generation = store.generation().unwrap();
    let c = Coordinator::new(store, None);
    let reference = search(&c, "durable")[0]["reference"].clone();
    drop(c);
    let connection =
        rusqlite::Connection::open(temporary.path().join("data/store.sqlite")).unwrap();
    connection.execute_batch("UPDATE collection_policies SET policy=json_remove(policy,'$.watch'); PRAGMA user_version=5;").unwrap();
    drop(connection);
    let store = Store::open(&temporary.path().join("data"), None).unwrap();
    assert!(!store.collection_policy("fixture").unwrap().watch);
    assert_eq!(
        store
            .context(&serde_json::from_value(reference).unwrap())
            .unwrap(),
        "durable policy\n"
    );
    assert_eq!(store.generation().unwrap(), generation);
    store.integrity().unwrap();
}

#[test]
fn a_full_watch_budget_keeps_periodic_recovery_and_queries_available() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    for index in 0..512 {
        fs::write(source.join(format!("{index}.md")), "ordinary fixture\n").unwrap();
    }
    fs::write(source.join("guide.md"), "beforefallback\n").unwrap();
    let c = Coordinator::new(
        Store::open(&temporary.path().join("data"), None).unwrap(),
        None,
    );
    let id=call(&c,json!({"operation":"register","name":"fixture","root":source,"policy":{"watch":true,"reconcileIntervalMs":1000}}))["collectionId"].as_str().unwrap().to_owned();
    wait_for(|| !search(&c, "beforefallback").as_array().unwrap().is_empty());
    assert_eq!(status(&c)["watch"]["collections"][&id]["active"], false);
    assert_eq!(
        status(&c)["watch"]["collections"][&id]["lastErrorCode"],
        "resource_exhausted"
    );
    fs::write(source.join("guide.md"), "afterfallback\n").unwrap();
    wait_for(|| !search(&c, "afterfallback").as_array().unwrap().is_empty());
    assert!(search(&c, "beforefallback").as_array().unwrap().is_empty());
    call(&c, json!({"operation":"integrity"}));
}
