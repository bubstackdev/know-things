use know_things::{contracts::*, store::Store, transport};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

#[test]
fn real_cli_roundtrip_and_mcp_share_evidence_identity() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("secret")).unwrap();
    fs::write(root.join("secret/invalid.txt"), [255]).unwrap();
    fs::write(
        root.join("guide.md"),
        "# Policy\n\nRedis PersistentCache migration guidance.\n",
    )
    .unwrap();
    let data = temp.path().join("data");
    let requests = [
        json!({"schemaVersion":1,"requestId":"register","operation":"register","name":"kb","root":root,"policy":{"excludedPaths":["secret"]}}),
        json!({"schemaVersion":1,"requestId":"ingest","operation":"reconcile","collection":"kb"}),
        json!({"schemaVersion":1,"requestId":"rebuild","operation":"rebuild_lexical","deadlineMs":5000}),
        json!({"schemaVersion":1,"requestId":"search","operation":"search","query":"PersistentCache","collections":["kb"],"mode":"lexical"}),
    ];
    let mut child = Command::new(env!("CARGO_BIN_EXE_know-things"))
        .args(["local", data.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut responses: Vec<Value> = Vec::new();
    for request in requests {
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        // Dependent requests wait for reconciliation's acknowledgment. Other
        // clients may retrieve the prior committed revision while it runs.
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        responses.push(serde_json::from_str(&line).unwrap());
    }
    drop(input);
    assert!(child.wait().unwrap().success());
    assert_eq!(responses.len(), 4);
    for response in &responses {
        assert_eq!(response["status"], "ok", "{response}");
    }
    assert_eq!(responses[1]["data"]["filteredEntries"], 1);
    assert_eq!(responses[1]["data"]["failures"], json!([]));
    let reference: Reference =
        serde_json::from_value(responses[3]["data"]["evidence"][0]["reference"].clone()).unwrap();
    let messages = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"know_things","arguments":{"operation":"context","reference":reference}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"know_things","arguments":{"operation":"rebuild_lexical","deadlineMs":5000}}}),
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"know_things","arguments":{"operation":"collection_policy","collection":"kb"}}}),
        json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"know_things","arguments":{"operation":"set_collection_policy","collection":"kb","policy":{"watch":true,"excludedPaths":["omit","secret"],"retention":{"retainRevisions":2,"intervalMs":86400000}}}}}),
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"know_things","arguments":{"operation":"prune","retainRevisions":1,"maxRevisions":1,"deadlineMs":5000}}}),
    ];
    let lines = messages
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut child = Command::new(env!("CARGO_BIN_EXE_know-things"))
        .args(["mcp-stdio", data.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(lines.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let results: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(results.len(), 7);
    for id in [5, 6, 7] {
        let response = results.iter().find(|r| r["id"] == id).unwrap();
        assert_eq!(response["result"]["isError"], false);
        let envelope: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(envelope["status"], "ok");
        if id == 6 {
            assert_eq!(envelope["data"]["policy"]["watch"], true);
            assert_eq!(
                envelope["data"]["policy"]["retention"]["retainRevisions"],
                2
            );
            assert_eq!(envelope["data"]["policy"]["retention"]["maxRevisions"], 128);
            assert_eq!(envelope["data"]["policy"]["retention"]["deadlineMs"], 1000);
        }
    }
    let context = results.iter().find(|r| r["id"] == 3).unwrap();
    let rebuilt = results.iter().find(|r| r["id"] == 4).unwrap();
    assert_eq!(context["result"]["isError"], false);
    assert_eq!(rebuilt["result"]["isError"], false);
    let rebuild: Value =
        serde_json::from_str(rebuilt["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(rebuild["data"]["rebuilt"], "lexical");
    let envelope: Value =
        serde_json::from_str(context["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        envelope["data"]["reference"],
        serde_json::to_value(reference).unwrap()
    );
    assert!(
        envelope["data"]["text"]
            .as_str()
            .unwrap()
            .contains("PersistentCache")
    );
}
#[test]
fn framing_rejects_oversized_and_partial_messages() {
    assert!(
        transport::read_frame(&mut &((transport::MAX_FRAME as u32 + 1).to_be_bytes())[..]).is_err()
    );
    assert!(transport::read_frame(&mut &[0, 0, 0, 5, 1, 2][..]).is_err());
    let mut frame = Vec::new();
    transport::write_frame(&mut frame, b"hello").unwrap();
    assert_eq!(transport::read_frame(&mut &frame[..]).unwrap(), b"hello");
}
#[test]
fn newer_store_and_symlinked_database_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    drop(Store::open(&data, None).unwrap());
    let db = rusqlite::Connection::open(data.join("store.sqlite")).unwrap();
    db.pragma_update(None, "user_version", 99).unwrap();
    drop(db);
    assert_eq!(
        Store::open(&data, None).err().unwrap().code,
        "store_incompatible"
    );
    let other = temp.path().join("other");
    fs::create_dir(&other).unwrap();
    std::os::unix::fs::symlink(data.join("store.sqlite"), other.join("store.sqlite")).unwrap();
    assert!(Store::open(&other, None).is_err());
}

#[test]
fn embedding_startup_errors_preserve_lexical_service_and_explicit_hybrid_failure_in_cli_and_mcp() {
    for fault in ["missing", "tampered", "handshake"] {
        for mode in ["local", "mcp-stdio"] {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source");
            fs::create_dir(&source).unwrap();
            fs::write(source.join("guide.md"), "Durable policy.").unwrap();
            let data = temp.path().join("data");
            let mut store = Store::open(&data, None).unwrap();
            store.register("kb", &source).unwrap();
            store.reconcile("kb").unwrap();
            let reference = store
                .search(
                    serde_json::from_value(
                        json!({"query":"durable","collections":["kb"],"mode":"lexical"}),
                    )
                    .unwrap(),
                )
                .unwrap()
                .evidence[0]
                .reference
                .clone();
            let generation = store.generation().unwrap();
            drop(store);
            let model = temp.path().join("embedding");
            if fault != "missing" {
                fs::create_dir(&model).unwrap();
                let worker = model.join("worker.py");
                fs::write(&worker,r#"#!/usr/bin/env python3
import json,struct,sys,time
data=json.dumps(dict(protocolVersion=1,kind='embedding',manifestId='wrong',runtime='fixture')).encode()
sys.stdout.buffer.write(struct.pack('>I',len(data))+data);sys.stdout.buffer.flush()
time.sleep(60)
"#).unwrap();
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
                let artifacts = json!({"worker.py":if fault=="tampered" {"0".repeat(64)}else{know_things::assets::digest(&worker).unwrap()}});
                let manifest = json!({"name":"startup-failure-fixture","artifacts":artifacts,"tokenizer":"fixture","architecture":"fixture","quantization":"fixture","runtime":"fixture","queryPrefix":"","documentPrefix":"","pooling":"fixture","normalization":"l2","dimensions":2,"maxInputTokens":4096,"license":"fixture"});
                fs::write(model.join("bundle.json"),serde_json::to_vec(&json!({"schemaVersion":1,"kind":"embedding","entrypoint":"worker.py","arguments":[],"artifacts":artifacts,"manifest":manifest})).unwrap()).unwrap();
            }
            let mut child = Command::new(env!("CARGO_BIN_EXE_know-things"))
                .args([
                    mode,
                    data.to_str().unwrap(),
                    "--embedding-bundle",
                    model.to_str().unwrap(),
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut input = child.stdin.take().unwrap();
            let mut reader = BufReader::new(child.stdout.take().unwrap());
            if mode == "mcp-stdio" {
                writeln!(input,"{}",json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"startup-test","version":"1"}}})).unwrap();
                input.flush().unwrap();
                let mut line = String::new();
                assert!(
                    reader.read_line(&mut line).unwrap() > 0,
                    "service must start despite embedding error"
                );
                assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["id"], 0);
                writeln!(
                    input,
                    "{}",
                    json!({"jsonrpc":"2.0","method":"notifications/initialized"})
                )
                .unwrap();
            }
            let operations = [
                json!({"operation":"status"}),
                json!({"operation":"search","query":"durable","collections":["kb"],"mode":"lexical"}),
                json!({"operation":"search","query":"durable","collections":["kb"],"mode":"hybrid"}),
                json!({"operation":"search","query":"durable","collections":["kb"],"mode":"hybrid","strictHybrid":true}),
                json!({"operation":"context","reference":reference}),
                json!({"operation":"reconcile","collection":"kb"}),
                json!({"operation":"integrity"}),
            ];
            let mut responses = Vec::new();
            for (index, mut operation) in operations.into_iter().enumerate() {
                let request = if mode == "local" {
                    operation["schemaVersion"] = json!(1);
                    operation["requestId"] = json!(format!("request-{index}"));
                    operation
                } else {
                    json!({"jsonrpc":"2.0","id":index+1,"method":"tools/call","params":{"name":"know_things","arguments":operation}})
                };
                writeln!(input, "{request}").unwrap();
                input.flush().unwrap();
                let mut line = String::new();
                assert!(
                    reader.read_line(&mut line).unwrap() > 0,
                    "service must stay available"
                );
                let response: Value = serde_json::from_str(&line).unwrap();
                responses.push(if mode == "local" {
                    response
                } else {
                    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                        .unwrap()
                });
            }
            drop(input);
            assert!(child.wait_with_output().unwrap().status.success());
            let expected = match fault {
                "missing" => "io_error",
                "tampered" => "model_incompatible",
                _ => "model_incompatible",
            };
            assert_eq!(responses[0]["data"]["embeddingError"]["code"], expected);
            assert!(responses[0]["data"]["configuredEmbeddingSpaceId"].is_null());
            assert_eq!(responses[1]["data"]["actualMode"], "lexical");
            assert_eq!(
                responses[1]["data"]["evidence"][0]["reference"],
                serde_json::to_value(&reference).unwrap()
            );
            assert_eq!(responses[2]["data"]["actualMode"], "lexical");
            assert_eq!(
                responses[2]["data"]["degradationReasons"],
                json!(["embedding_model_unavailable"])
            );
            assert_eq!(responses[3]["error"]["code"], "model_unavailable");
            assert_eq!(responses[4]["data"]["text"], "Durable policy.");
            assert_eq!(responses[5]["data"]["unchanged"], 1);
            assert_eq!(responses[6]["status"], "ok");
            for response in responses {
                assert_eq!(response["corpusGeneration"], generation);
            }
        }
    }
}
#[test]
fn uncommitted_activation_is_rolled_back_after_abrupt_exit() {
    if let Ok(path) = std::env::var("KNOW_THINGS_CRASH_FIXTURE") {
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch("BEGIN IMMEDIATE; UPDATE documents SET active_revision='incomplete-revision'; DELETE FROM lexical; UPDATE meta SET value='999' WHERE key='generation';").unwrap();
        std::process::exit(0); // No destructors or transaction rollback in the child.
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    let data = temp.path().join("data");
    let mut store = Store::open(&data, None).unwrap();
    store.register("kb", &root).unwrap();
    let stage = store
        .stage("kb", "guide.md", "durable policy")
        .unwrap()
        .unwrap();
    store.activate(stage).unwrap();
    let generation = store.generation().unwrap();
    drop(store);
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "uncommitted_activation_is_rolled_back_after_abrupt_exit",
        ])
        .env("KNOW_THINGS_CRASH_FIXTURE", data.join("store.sqlite"))
        .status()
        .unwrap();
    assert!(status.success());
    let store = Store::open(&data, None).unwrap();
    assert_eq!(store.generation().unwrap(), generation);
    store.integrity().unwrap();
}
#[test]
fn malformed_stdio_request_does_not_poison_next_operation() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path(), None).unwrap();
    let mut output = Vec::new();
    transport::local(
        store,
        &b"not json\n{\"schemaVersion\":1,\"requestId\":\"ok\",\"operation\":\"status\"}\n"[..],
        &mut output,
    )
    .unwrap();
    let lines: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(lines[0]["status"], "error");
    assert_eq!(lines[1]["status"], "ok");
}
