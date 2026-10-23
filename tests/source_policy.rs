use know_things::{contracts::*, store::Store};
use std::fs;
mod support;
fn policy(paths: &[&str]) -> CollectionPolicy {
    CollectionPolicy {
        watch: false,
        excluded_paths: paths.iter().map(|s| s.to_string()).collect(),
        reconcile_interval_ms: None,
        retention: None,
    }
}
fn search(store: &mut Store, query: &str) -> EvidencePack {
    store
        .search(Search {
            query: query.into(),
            collections: vec!["kb".into()],
            top_k: 20,
            evidence_token_budget: 12000,
            deadline_ms: 5000,
            mode: Mode::Lexical,
            strict_hybrid: false,
        })
        .unwrap()
}
fn setup() -> (tempfile::TempDir, Store) {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("source")).unwrap();
    let mut store = Store::open(&temp.path().join("data"), None).unwrap();
    store.register("kb", &temp.path().join("source")).unwrap();
    (temp, store)
}

#[test]
fn collection_names_cannot_alias_another_collections_id() {
    let (temp, mut store) = setup();
    let id = store.register("kb", &temp.path().join("source")).unwrap();
    let other = temp.path().join("other");
    fs::create_dir(&other).unwrap();
    assert_eq!(
        store.register(&id, &other).unwrap_err().code,
        "invalid_request"
    );
    assert_eq!(store.collection_policy(&id).unwrap(), policy(&[]));
    store.integrity().unwrap();
}

#[test]
fn schema_two_upgrade_preserves_references_and_generation_and_defaults_policy_off() {
    let (temp, mut store) = setup();
    fs::write(temp.path().join("source/guide.md"), "durable policy").unwrap();
    store.reconcile("kb").unwrap();
    let reference = search(&mut store, "durable").evidence[0].reference.clone();
    let generation = store.generation().unwrap();
    drop(store);
    let connection = rusqlite::Connection::open(temp.path().join("data/store.sqlite")).unwrap();
    // Reconstruct the preceding schema without changing any canonical document rows.
    support::legacy_vectors(&connection);
    connection
        .execute_batch("DROP TABLE collection_policies; PRAGMA user_version=2;")
        .unwrap();
    drop(connection);
    let mut upgraded = Store::open(&temp.path().join("data"), None).unwrap();
    assert_eq!(upgraded.generation().unwrap(), generation);
    assert_eq!(upgraded.collection_policy("kb").unwrap(), policy(&[]));
    assert_eq!(upgraded.context(&reference).unwrap(), "durable policy");
    assert_eq!(
        search(&mut upgraded, "durable").evidence[0].reference,
        reference
    );
    upgraded.integrity().unwrap();
}
#[test]
fn registration_filters_before_reading_or_parsing_and_does_not_reset_existing_policy() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("secret")).unwrap();
    fs::write(source.join("secret/invalid.txt"), [255, 254]).unwrap();
    fs::write(source.join("omit.txt"), [255]).unwrap();
    fs::write(source.join("secret-notes.md"), "public policy").unwrap();
    let mut store = Store::open(&temp.path().join("data"), None).unwrap();
    store
        .register_with_policy(
            "kb",
            &source,
            Some(policy(&["secret", "omit.txt", "secret"])),
        )
        .unwrap();
    let report = store.reconcile("kb").unwrap();
    assert!(report.failures.is_empty());
    assert_eq!(report.active, 1);
    assert_eq!(report.filtered_entries, 2);
    assert_eq!(
        search(&mut store, "policy").evidence[0].source,
        "secret-notes.md"
    );
    assert!(
        store
            .stage("kb", "secret/new.md", "must never be staged")
            .unwrap()
            .is_none()
    );
    store.register("kb", &source).unwrap();
    assert_eq!(
        store.collection_policy("kb").unwrap(),
        policy(&["omit.txt", "secret"])
    );
    assert!(
        store
            .register_with_policy("kb", &source, Some(policy(&[])))
            .is_err()
    );
    store.integrity().unwrap();
}
#[test]
fn tightening_and_relaxing_policy_never_revives_late_jobs_or_overrides_explicit_exclusion() {
    let (temp, mut store) = setup();
    fs::create_dir(temp.path().join("source/private")).unwrap();
    fs::write(
        temp.path().join("source/private/guide.md"),
        "confidential policy",
    )
    .unwrap();
    store.reconcile("kb").unwrap();
    let reference = search(&mut store, "confidential").evidence[0]
        .reference
        .clone();
    let late = store
        .stage("kb", "private/guide.md", "late replacement")
        .unwrap()
        .unwrap();
    store
        .set_collection_policy("kb", policy(&["private"]))
        .unwrap();
    assert!(search(&mut store, "confidential").evidence.is_empty());
    assert_eq!(
        store.context(&reference).unwrap_err().code,
        "source_unavailable"
    );
    assert!(store.include(&reference.document_id).is_err());
    store.set_collection_policy("kb", policy(&[])).unwrap();
    assert!(!store.activate(late).unwrap());
    assert!(search(&mut store, "confidential").evidence.is_empty());
    store.reconcile("kb").unwrap();
    assert_eq!(store.context(&reference).unwrap(), "confidential policy");
    store.exclude(&reference.document_id).unwrap();
    store
        .set_collection_policy("kb", policy(&["private"]))
        .unwrap();
    store.set_collection_policy("kb", policy(&[])).unwrap();
    store.reconcile("kb").unwrap();
    assert!(store.context(&reference).is_err());
    store.include(&reference.document_id).unwrap();
    store.reconcile("kb").unwrap();
    assert_eq!(store.context(&reference).unwrap(), "confidential policy");
    assert_eq!(
        fs::read_to_string(temp.path().join("source/private/guide.md")).unwrap(),
        "confidential policy"
    );
    store.integrity().unwrap();
    drop(store);
    let mut reopened = Store::open(&temp.path().join("data"), None).unwrap();
    assert_eq!(search(&mut reopened, "confidential").evidence.len(), 1);
}
#[test]
fn invalid_rules_and_intervals_do_not_mutate_policy_or_generation() {
    let (_temp, mut store) = setup();
    let generation = store.generation().unwrap();
    for path in [
        "",
        "/absolute",
        "../escape",
        "a/../b",
        "a//b",
        "a/",
        "a\\b",
        "*.md",
        "nul\0path",
    ] {
        assert_eq!(
            store
                .set_collection_policy("kb", policy(&[path]))
                .unwrap_err()
                .code,
            "invalid_request"
        );
    }
    for ms in [0, 999, 86400001, u64::MAX] {
        let mut invalid = policy(&[]);
        invalid.reconcile_interval_ms = Some(ms);
        assert_eq!(
            store.set_collection_policy("kb", invalid).unwrap_err().code,
            "invalid_request"
        );
    }
    assert_eq!(store.collection_policy("kb").unwrap(), policy(&[]));
    assert_eq!(store.generation().unwrap(), generation);
}
#[test]
fn failed_policy_transactions_preserve_eligibility_and_configuration() {
    for exhausted in ["desired", "generation"] {
        let (temp, mut store) = setup();
        fs::write(temp.path().join("source/guide.md"), "policy").unwrap();
        store.reconcile("kb").unwrap();
        let reference = search(&mut store, "policy").evidence[0].reference.clone();
        drop(store);
        let connection = rusqlite::Connection::open(temp.path().join("data/store.sqlite")).unwrap();
        if exhausted == "desired" {
            connection
                .execute("UPDATE documents SET desired=?1", [i64::MAX])
                .unwrap();
        } else {
            connection
                .execute(
                    "UPDATE meta SET value=?1 WHERE key='generation'",
                    [i64::MAX.to_string()],
                )
                .unwrap();
        }
        drop(connection);
        let mut store = Store::open(&temp.path().join("data"), None).unwrap();
        let generation = store.generation().unwrap();
        assert_eq!(
            store
                .set_collection_policy("kb", policy(&["guide.md"]))
                .unwrap_err()
                .code,
            "resource_exhausted"
        );
        assert_eq!(store.collection_policy("kb").unwrap(), policy(&[]));
        assert_eq!(store.generation().unwrap(), generation);
        assert_eq!(store.context(&reference).unwrap(), "policy");
        store.integrity().unwrap();
    }
}
#[test]
fn policies_survive_backup_restore_with_their_exclusion_boundary() {
    let (temp, mut store) = setup();
    fs::write(temp.path().join("source/guide.md"), "policy").unwrap();
    store.reconcile("kb").unwrap();
    let reference = search(&mut store, "policy").evidence[0].reference.clone();
    store
        .set_collection_policy("kb", policy(&["guide.md"]))
        .unwrap();
    let backup = temp.path().join("backup.sqlite");
    store.backup(&backup).unwrap();
    let mut restored = Store::restore(&backup, &temp.path().join("restored")).unwrap();
    assert_eq!(
        restored.collection_policy("kb").unwrap(),
        policy(&["guide.md"])
    );
    restored.reconcile("kb").unwrap();
    assert!(restored.context(&reference).is_err());
    restored.integrity().unwrap();
}

fn op(c: &know_things::coordinator::Coordinator, value: serde_json::Value) -> Response {
    let mut value = value;
    value["schemaVersion"] = serde_json::json!(1);
    value["requestId"] = serde_json::json!("test");
    c.execute(serde_json::from_value(value).unwrap())
}
fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    while !ready() {
        assert!(
            std::time::Instant::now() < deadline,
            "bounded periodic observation"
        );
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
}
fn cron_query(c: &know_things::coordinator::Coordinator, query: &str) -> serde_json::Value {
    let result = op(
        c,
        serde_json::json!({"operation":"search","query":query,"collections":["kb"],"mode":"lexical"}),
    );
    assert_eq!(result.status, "ok");
    result.data.unwrap()
}
#[test]
fn periodic_reconciliation_discovers_edits_and_removals_can_be_disabled_and_shuts_down() {
    use know_things::coordinator::Coordinator;
    use serde_json::json;
    let (temp, mut store) = setup();
    let source = temp.path().join("source");
    fs::create_dir(source.join("secret")).unwrap();
    fs::write(source.join("secret/invalid.txt"), [255]).unwrap();
    fs::write(source.join("guide.md"), "first policy").unwrap();
    let mut configured = policy(&["secret"]);
    configured.reconcile_interval_ms = Some(1000);
    store.set_collection_policy("kb", configured).unwrap();
    let c = Coordinator::new(store, None);
    wait_for(|| {
        !cron_query(&c, "first")["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    });
    let old = cron_query(&c, "first")["evidence"][0]["reference"].clone();
    fs::write(source.join("guide.md"), "second policy").unwrap();
    wait_for(|| {
        !cron_query(&c, "second")["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    });
    assert!(
        cron_query(&c, "first")["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        op(&c, json!({"operation":"context","reference":old}))
            .data
            .unwrap()["text"],
        "first policy"
    );
    fs::remove_file(source.join("guide.md")).unwrap();
    wait_for(|| {
        cron_query(&c, "second")["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    });
    assert!(
        op(&c, json!({"operation":"context","reference":old}))
            .error
            .is_some()
    );
    let result = op(
        &c,
        json!({"operation":"set_collection_policy","collection":"kb","policy":{"excludedPaths":["secret"]}}),
    );
    assert_eq!(result.status, "ok");
    wait_for(|| {
        op(&c, json!({"operation":"status"})).data.unwrap()["periodicReconciliation"]["configuredCollections"]
            == 0
    });
    fs::write(source.join("guide.md"), "disabled policy").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert!(
        cron_query(&c, "disabled")["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        op(&c, json!({"operation":"reconcile","collection":"kb"})).status,
        "ok"
    );
    assert!(
        !cron_query(&c, "disabled")["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Long future timers must be woken by shutdown, rather than retaining ownership.
    assert_eq!(op(&c,json!({"operation":"set_collection_policy","collection":"kb","policy":{"excludedPaths":["secret"],"reconcileIntervalMs":86400000}})).status,"ok");
    wait_for(|| {
        op(&c, json!({"operation":"status"})).data.unwrap()["periodicReconciliation"]["configuredCollections"]
            == 1
    });
    let stopped = std::time::Instant::now();
    drop(c);
    assert!(stopped.elapsed() < std::time::Duration::from_secs(2));
    Store::open(&temp.path().join("data"), None)
        .unwrap()
        .integrity()
        .unwrap();
}
#[test]
fn persisted_periodic_policy_resumes_after_restart() {
    use know_things::coordinator::Coordinator;
    let (temp, mut store) = setup();
    let mut configured = policy(&[]);
    configured.reconcile_interval_ms = Some(1000);
    store
        .set_collection_policy("kb", configured.clone())
        .unwrap();
    drop(store);
    let reopened = Store::open(&temp.path().join("data"), None).unwrap();
    assert_eq!(reopened.collection_policy("kb").unwrap(), configured);
    fs::write(temp.path().join("source/guide.md"), "resumed policy").unwrap();
    let c = Coordinator::new(reopened, None);
    wait_for(|| {
        !cron_query(&c, "resumed")["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    });
}
