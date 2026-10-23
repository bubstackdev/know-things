use know_things::{
    assets,
    embedding::{Embedder, LocalEmbedder, Manifest},
    process::Worker,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    process::Command,
    time::{Duration, Instant},
};
fn bundle(root: &std::path::Path) -> Manifest {
    let script = root.join("worker.py");
    // A conspicuously labeled fixture, never a semantic-quality model.
    fs::write(&script,r##"#!/usr/bin/env python3
import hashlib,json,os,struct,sys
m=json.load(open(os.path.join(os.environ['KNOW_THINGS_ASSET_ROOT'],'bundle.json')))['manifest']
def send(v):
 b=json.dumps(v).encode();sys.stdout.buffer.write(struct.pack('>I',len(b))+b);sys.stdout.buffer.flush()
send(dict(protocolVersion=1,kind='embedding',manifestId=hashlib.sha256(json.dumps(m,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode()).hexdigest(),runtime=m['runtime']))
while True:
 b=sys.stdin.buffer.read(4)
 if not b:break
 r=json.loads(sys.stdin.buffer.read(struct.unpack('>I',b)[0]))
 send(dict(requestId=r['requestId'],status='ok',vector=[1,0]))
"##).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let artifacts = BTreeMap::from([("worker.py".into(), assets::digest(&script).unwrap())]);
    let manifest = Manifest {
        name: "fixture-not-a-semantic-model".into(),
        artifacts: artifacts.clone(),
        tokenizer: "fixture".into(),
        architecture: "fixture".into(),
        quantization: "f32".into(),
        runtime: "stdlib-fixture".into(),
        query_prefix: "".into(),
        document_prefix: "".into(),
        pooling: "fixture".into(),
        normalization: "l2".into(),
        dimensions: 2,
        max_input_tokens: 100,
        license: "fixture".into(),
    };
    fs::write(root.join("bundle.json"),serde_json::to_vec(&json!({"schemaVersion":1,"kind":"embedding","entrypoint":"worker.py","arguments":[],"artifacts":artifacts,"manifest":manifest})).unwrap()).unwrap();
    manifest
}
#[test]
fn verified_embedding_process_and_offline_asset_installation() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let manifest = bundle(&source);
    let destination = temp.path().join("installed");
    assets::import(&source, &destination).unwrap();
    assert!(assets::import(&source, &destination).is_err());
    let mut embedder = LocalEmbedder::open(&destination).unwrap();
    assert_eq!(embedder.manifest().space_id(), manifest.space_id());
    assert_eq!(
        embedder
            .embed("hello", true, Instant::now() + Duration::from_secs(2))
            .unwrap(),
        vec![1., 0.]
    );
    drop(embedder);
    fs::write(destination.join("worker.py"), "tampered").unwrap();
    assert!(assets::verify(&destination).is_err());
}
#[test]
fn worker_timeout_is_bounded_and_instance_cannot_be_reused() {
    let mut command = Command::new("python3");
    command.args(["-c", "import time;time.sleep(60)"]);
    let mut worker = Worker::spawn(&mut command).unwrap();
    let started = Instant::now();
    let result = worker.receive::<serde_json::Value>(Instant::now() + Duration::from_millis(100));
    assert_eq!(result.unwrap_err().code, "deadline_exceeded");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(
        worker
            .receive::<serde_json::Value>(Instant::now() + Duration::from_secs(1))
            .unwrap_err()
            .code,
        "worker_failed"
    );
}

#[test]
fn healthy_worker_drop_closes_input_and_waits_for_runtime_finalizers() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("finalized");
    let mut command = Command::new("python3");
    command.args(["-c",r#"import atexit,json,struct,sys,time
def finalize():
 time.sleep(.15)
 with open(sys.argv[1],'w') as stream:stream.write('finalized after EOF')
atexit.register(finalize)
data=json.dumps({'ready':True}).encode();sys.stdout.buffer.write(struct.pack('>I',len(data))+data);sys.stdout.buffer.flush()
sys.stdin.buffer.read()
"#,marker.to_str().unwrap()]);
    let mut worker = Worker::spawn(&mut command).unwrap();
    assert_eq!(
        worker
            .receive::<serde_json::Value>(Instant::now() + Duration::from_secs(2))
            .unwrap()["ready"],
        true
    );
    let started = Instant::now();
    drop(worker);
    assert_eq!(fs::read_to_string(marker).unwrap(), "finalized after EOF");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn healthy_but_unresponsive_worker_drop_has_a_bounded_eof_grace() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("eof");
    let mut command = Command::new("python3");
    command.args(["-c",r#"import json,struct,sys,time
data=json.dumps({'ready':True}).encode();sys.stdout.buffer.write(struct.pack('>I',len(data))+data);sys.stdout.buffer.flush()
sys.stdin.buffer.read()
with open(sys.argv[1],'w') as stream:stream.write('EOF received')
time.sleep(60)
"#,marker.to_str().unwrap()]);
    let mut worker = Worker::spawn(&mut command).unwrap();
    worker
        .receive::<serde_json::Value>(Instant::now() + Duration::from_secs(2))
        .unwrap();
    let started = Instant::now();
    drop(worker);
    assert_eq!(fs::read_to_string(marker).unwrap(), "EOF received");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn explicit_stop_remains_immediate_and_idempotent_without_healthy_drop_grace() {
    let mut command = Command::new("python3");
    command.args(["-c",r#"import json,struct,sys,time
data=json.dumps({'ready':True}).encode();sys.stdout.buffer.write(struct.pack('>I',len(data))+data);sys.stdout.buffer.flush()
time.sleep(60)
"#]);
    let mut worker = Worker::spawn(&mut command).unwrap();
    worker
        .receive::<serde_json::Value>(Instant::now() + Duration::from_secs(2))
        .unwrap();
    let started = Instant::now();
    worker.stop();
    worker.stop();
    drop(worker);
    assert!(started.elapsed() < Duration::from_millis(800));
}
#[test]
fn worker_crash_and_malformed_frames_fail_without_replay() {
    for code in [
        "import sys;sys.exit(1)",
        "import sys;sys.stdout.buffer.write(b'\\x7f\\xff\\xff\\xff');sys.stdout.buffer.flush()",
    ] {
        let mut command = Command::new("python3");
        command.args(["-c", code]);
        let mut worker = Worker::spawn(&mut command).unwrap();
        assert_eq!(
            worker
                .receive::<serde_json::Value>(Instant::now() + Duration::from_secs(2))
                .unwrap_err()
                .code,
            "worker_failed"
        );
    }
}
#[test]
fn asset_paths_cannot_escape_the_bundle() {
    assert!(assets::relative("../secret").is_err());
    assert!(assets::relative("/secret").is_err());
    assert!(assets::relative("safe/model.json").is_ok());
    let temp = tempfile::tempdir().unwrap();
    bundle(temp.path());
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(temp.path().join("bundle.json")).unwrap()).unwrap();
    config["artifacts"] = json!({"../secret":"0".repeat(64)});
    fs::write(
        temp.path().join("bundle.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    assert!(assets::verify(temp.path()).is_err());
}
#[test]
fn larger_on_disk_runtime_manifests_remain_bounded_independently_of_ipc_frames() {
    let temp = tempfile::tempdir().unwrap();
    let payload = temp.path().join("payload");
    fs::write(&payload, b"pinned bytes").unwrap();
    let mut config = json!({"schemaVersion":1,"kind":"embedding_runtime","entrypoint":"payload","arguments":[],"artifacts":{"payload":assets::digest(&payload).unwrap()},"manifest":{"inventoryNotes":"x".repeat(2*1024*1024)}});
    fs::write(
        temp.path().join("bundle.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    assert!(assets::verify(temp.path()).is_ok());
    config["manifest"]["inventoryNotes"] = json!("x".repeat(9 * 1024 * 1024));
    fs::write(
        temp.path().join("bundle.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    assert_eq!(
        assets::verify(temp.path()).unwrap_err().code,
        "model_incompatible"
    );
    assert_eq!(know_things::transport::MAX_FRAME, 1024 * 1024);
}
#[test]
fn control_channel_remains_usable_when_event_pipe_is_full() {
    // Child's event writer blocks; its independent control reader can still exit.
    let code = r#"import os,struct,threading
fd=int(os.environ['KNOW_THINGS_CONTROL_FD'])
def control():
 header=os.read(fd,4)
 if len(header)==4:
  count=struct.unpack('>I',header)[0]
  if os.read(fd,count):os._exit(0)
threading.Thread(target=control,daemon=True).start()
while True:os.write(1,b'x'*65536)
"#;
    let mut command = Command::new("python3");
    command.args(["-c", code]);
    let (mut worker, control) = Worker::spawn_with_control(command).unwrap();
    control
        .send(
            &json!({"operation":"cancel","requestId":1}),
            Instant::now() + Duration::from_millis(500),
        )
        .unwrap();
    assert!(
        worker
            .receive::<serde_json::Value>(Instant::now() + Duration::from_secs(2))
            .is_err()
    );
}
