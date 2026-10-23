//! Long-running embedding must not own the canonical store or defeat lifecycle fences.
use know_things::{
    contracts::*,
    coordinator::Coordinator,
    embedding::{Embedder, Manifest, SharedEmbedder},
    store::Store,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Gate {
    state: Mutex<(usize, bool)>,
    changed: Condvar,
}
impl Gate {
    fn wait(&self) {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |s| s.0 == 0)
            .unwrap();
        assert!(state.0 > 0, "embedding must have started");
    }
    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Slow {
    manifest: Manifest,
    gate: Arc<Gate>,
}
impl Slow {
    fn new(gate: Arc<Gate>) -> Self {
        Self {
            gate,
            manifest: Manifest {
                name: "ingestion-fixture".into(),
                artifacts: BTreeMap::new(),
                tokenizer: "fixture".into(),
                architecture: "fixture".into(),
                quantization: "f32".into(),
                runtime: "fixture".into(),
                query_prefix: "".into(),
                document_prefix: "".into(),
                pooling: "fixture".into(),
                normalization: "l2".into(),
                dimensions: 2,
                max_input_tokens: 4096,
                license: "fixture".into(),
            },
        }
    }
}
impl Embedder for Slow {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    fn embed(&mut self, _: &str, _: bool, deadline: Instant) -> Result<Vec<f32>> {
        let mut state = self.gate.state.lock().unwrap();
        state.0 += 1;
        self.gate.changed.notify_all();
        while !state.1 {
            let timeout = deadline.saturating_duration_since(Instant::now());
            if timeout.is_zero() {
                return Err(Error::new("deadline_exceeded", "fixture deadline"));
            }
            state = self.gate.changed.wait_timeout(state, timeout).unwrap().0;
        }
        Ok(vec![1., 0.])
    }
}
fn request(operation: Value) -> Request {
    let mut value = operation;
    value["schemaVersion"] = json!(1);
    value["requestId"] = json!("ingestion-test");
    serde_json::from_value(value).unwrap()
}
fn call(c: &Coordinator, operation: Value) -> Response {
    c.execute(request(operation))
}

#[cfg(target_os = "macos")]
#[test]
fn native_changes_wait_behind_manual_ingestion_and_reconcile_the_latest_source() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("guide.md"), "original policy\n").unwrap();
    let gate = Arc::new(Gate::default());
    let mut store = Store::open(
        &temporary.path().join("data"),
        Some(Box::new(Slow::new(gate.clone()))),
    )
    .unwrap();
    let id = store.register("kb", &source).unwrap();
    let coordinator = Coordinator::new(store, None);
    let _release = Release(gate.clone());
    let worker = coordinator.clone();
    let join = std::thread::spawn(move || {
        call(&worker, json!({"operation":"reconcile","collection":"kb"}))
    });
    gate.wait();
    assert_eq!(call(&coordinator,json!({"operation":"set_collection_policy","collection":"kb","policy":{"watch":true,"reconcileIntervalMs":86400000}})).status,"ok");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = call(&coordinator, json!({"operation":"status"}))
            .data
            .unwrap();
        if status["periodicReconciliation"]["collections"][&id]["busySkips"]
            .as_u64()
            .unwrap_or(0)
            > 0
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    for index in 0..20 {
        fs::write(source.join("guide.md"), format!("revision{index} policy\n")).unwrap();
    }
    loop {
        let status = call(&coordinator, json!({"operation":"status"}))
            .data
            .unwrap();
        if status["periodicReconciliation"]["watch"]["observedEvents"]
            .as_u64()
            .unwrap_or(0)
            > 0
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    gate.release();
    let original = join.join().unwrap();
    assert_eq!(original.status, "ok");
    assert_eq!(
        original.data.unwrap()["failures"][0]["code"],
        "source_changed"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let response = search(&coordinator);
        assert_eq!(response.status, "ok");
        if response.data.unwrap()["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["text"] == "revision19 policy\n")
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        call(&coordinator, json!({"operation":"integrity"})).status,
        "ok"
    );
}
fn search(c: &Coordinator) -> Response {
    call(
        c,
        json!({"operation":"search","collections":["kb"],"query":"policy","mode":"lexical"}),
    )
}
#[test]
fn queries_and_exclusion_continue_during_embedding_and_stale_work_stops() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("guide.md"), "Original policy.").unwrap();
    let mut store = Store::open(&temp.path().join("data"), None).unwrap();
    store.register("kb", &source).unwrap();
    store.reconcile("kb").unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    store
        .attach_embedder(Box::new(Slow::new(gate.clone())))
        .unwrap();
    let coordinator = Coordinator::new(store, None);
    let document =
        search(&coordinator).data.unwrap()["evidence"][0]["reference"]["documentId"].clone();
    fs::write(
        source.join("guide.md"),
        "Replacement policy.\n\nSecond paragraph needs another embedding.",
    )
    .unwrap();
    let worker = coordinator.clone();
    let (done_tx, done_rx) = mpsc::channel();
    let join = std::thread::spawn(move || {
        done_tx
            .send(call(
                &worker,
                json!({"operation":"reconcile","collection":"kb"}),
            ))
            .unwrap();
    });
    gate.wait();
    let foreground = coordinator.clone();
    let (tx, rx) = mpsc::channel();
    let foreground_join = std::thread::spawn(move || {
        let search = search(&foreground);
        let busy = call(
            &foreground,
            json!({"operation":"reconcile","collection":"kb"}),
        );
        let exclude = call(
            &foreground,
            json!({"operation":"exclude","document":document}),
        );
        tx.send((search, busy, exclude)).unwrap();
    });
    let (retrieved, busy, exclude) = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("foreground must proceed while embedding is blocked");
    assert_eq!(retrieved.status, "ok");
    assert_eq!(
        retrieved.data.unwrap()["evidence"][0]["text"],
        "Original policy."
    );
    assert_eq!(busy.error.unwrap().code, "busy");
    assert_eq!(exclude.status, "ok");
    gate.release();
    let report = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(report.status, "ok");
    let report = report.data.unwrap();
    assert_eq!(report["active"], 0);
    assert_eq!(report["superseded"], 1);
    assert_eq!(report["failures"], json!([]));
    assert_eq!(
        gate.state.lock().unwrap().0,
        1,
        "stop stale work before another chunk"
    );
    assert!(
        search(&coordinator).data.unwrap()["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        call(&coordinator, json!({"operation":"integrity"})).status,
        "ok"
    );
    join.join().unwrap();
    foreground_join.join().unwrap();
}
#[test]
fn an_embedding_waiter_honors_its_own_deadline() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let embedding = Arc::new(SharedEmbedder::new(Box::new(Slow::new(gate.clone()))));
    let background = embedding.clone();
    let join = std::thread::spawn(move || {
        background.embed("document", false, Instant::now() + Duration::from_secs(2))
    });
    gate.wait();
    let started = Instant::now();
    assert_eq!(
        embedding
            .embed("query", true, started + Duration::from_millis(20))
            .unwrap_err()
            .code,
        "deadline_exceeded"
    );
    assert!(started.elapsed() < Duration::from_millis(500));
    gate.release();
    join.join().unwrap().unwrap();
}

#[test]
fn policy_tightening_stops_embedding_publication_and_cannot_be_undone_by_relaxation() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("guide.md"), "Original policy.").unwrap();
    let mut store = Store::open(&temp.path().join("data"), None).unwrap();
    store.register("kb", &source).unwrap();
    store.reconcile("kb").unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    store
        .attach_embedder(Box::new(Slow::new(gate.clone())))
        .unwrap();
    let coordinator = Coordinator::new(store, None);
    fs::write(
        source.join("guide.md"),
        "Replacement policy.\n\nAnother embedding unit.",
    )
    .unwrap();
    let worker = coordinator.clone();
    let join = std::thread::spawn(move || {
        call(&worker, json!({"operation":"reconcile","collection":"kb"}))
    });
    gate.wait();
    for paths in [json!(["guide.md"]), json!([])] {
        let response = call(
            &coordinator,
            json!({"operation":"set_collection_policy","collection":"kb","policy":{"excludedPaths":paths}}),
        );
        assert_eq!(response.status, "ok");
    }
    gate.release();
    let report = join.join().unwrap().data.unwrap();
    assert_eq!(report["superseded"], 1);
    assert_eq!(report["active"], 0);
    assert_eq!(report["failures"], json!([]));
    assert_eq!(gate.state.lock().unwrap().0, 1);
    assert!(
        search(&coordinator).data.unwrap()["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        call(&coordinator, json!({"operation":"integrity"})).status,
        "ok"
    );
}

#[test]
fn periodic_scans_and_retention_skip_busy_manual_ingestion() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("guide.md"), "Original policy.").unwrap();
    let mut store = Store::open(&temp.path().join("data"), None).unwrap();
    let id = store.register("kb", &source).unwrap();
    store.reconcile("kb").unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    store
        .attach_embedder(Box::new(Slow::new(gate.clone())))
        .unwrap();
    let coordinator = Coordinator::new(store, None);
    fs::write(source.join("guide.md"), "Replacement policy.").unwrap();
    let worker = coordinator.clone();
    let join = std::thread::spawn(move || {
        call(&worker, json!({"operation":"reconcile","collection":"kb"}))
    });
    gate.wait();
    assert_eq!(call(&coordinator, json!({"operation":"set_collection_policy","collection":"kb","policy":{"reconcileIntervalMs":1000,"retention":{"retainRevisions":1,"intervalMs":1000}}})).status, "ok");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = call(&coordinator, json!({"operation":"status"}))
            .data
            .unwrap();
        if ["collections", "retention"].iter().all(|kind| {
            status["periodicReconciliation"][kind][&id]["busySkips"]
                .as_u64()
                .unwrap_or(0)
                > 0
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "periodic attempt must report busy"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(gate.state.lock().unwrap().0, 1);
    assert_eq!(
        call(
            &coordinator,
            json!({"operation":"set_collection_policy","collection":"kb","policy":{}})
        )
        .status,
        "ok"
    );
    gate.release();
    let response = join.join().unwrap();
    assert_eq!(response.status, "ok");
    assert_eq!(response.data.unwrap()["active"], 1);
    assert_eq!(
        search(&coordinator).data.unwrap()["evidence"][0]["text"],
        "Replacement policy."
    );
}

struct Input {
    receiver: mpsc::Receiver<Vec<u8>>,
    buffered: std::io::Cursor<Vec<u8>>,
}
impl std::io::Read for Input {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        while self.buffered.position() as usize == self.buffered.get_ref().len() {
            match self.receiver.recv() {
                Ok(bytes) => self.buffered = std::io::Cursor::new(bytes),
                Err(_) => return Ok(0),
            }
        }
        std::io::Read::read(&mut self.buffered, output)
    }
}
struct Output {
    sender: mpsc::Sender<Value>,
    buffered: Vec<u8>,
}
impl std::io::Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffered.extend(bytes);
        while let Some(end) = self.buffered.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = self.buffered.drain(..=end).collect();
            self.sender
                .send(serde_json::from_slice(&line).unwrap())
                .unwrap();
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[test]
fn resident_ndjson_keeps_reading_requests_during_reconciliation() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("guide.md"), "Original policy.").unwrap();
    let mut store = Store::open(&temp.path().join("data"), None).unwrap();
    store.register("kb", &source).unwrap();
    store.reconcile("kb").unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    store
        .attach_embedder(Box::new(Slow::new(gate.clone())))
        .unwrap();
    fs::write(source.join("guide.md"), "Replacement policy.").unwrap();
    let (input_tx, input_rx) = mpsc::channel();
    let (output_tx, output_rx) = mpsc::channel();
    let join = std::thread::spawn(move || {
        know_things::transport::local(
            store,
            Input {
                receiver: input_rx,
                buffered: std::io::Cursor::new(Vec::new()),
            },
            Output {
                sender: output_tx,
                buffered: Vec::new(),
            },
        )
    });
    let send = |id: &str, operation: Value| {
        let mut operation = operation;
        operation["schemaVersion"] = json!(1);
        operation["requestId"] = json!(id);
        input_tx
            .send(format!("{operation}\n").into_bytes())
            .unwrap();
    };
    send("ingest", json!({"operation":"reconcile","collection":"kb"}));
    gate.wait();
    send(
        "search",
        json!({"operation":"search","query":"policy","collections":["kb"],"mode":"lexical"}),
    );
    let result = output_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("stdio reader must continue during ingestion");
    assert_eq!(result["requestId"], "search");
    assert_eq!(result["data"]["evidence"][0]["text"], "Original policy.");
    send(
        "duplicate",
        json!({"operation":"reconcile","collection":"kb"}),
    );
    let result = output_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(result["requestId"], "duplicate");
    assert_eq!(result["error"]["code"], "busy");
    gate.release();
    let result = output_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(result["requestId"], "ingest");
    assert_eq!(result["data"]["active"], 1);
    drop(input_tx);
    join.join().unwrap().unwrap();
}

#[test]
fn sources_replaced_by_fifos_or_parent_symlinks_cannot_block_or_escape_publication() {
    use std::os::unix::{ffi::OsStrExt, fs::symlink};
    for replacement in ["fifo", "parent_symlink"] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("sub")).unwrap();
        let path = source.join("sub/guide.md");
        fs::write(&path, "Original policy.").unwrap();
        let mut store = Store::open(&temp.path().join("data"), None).unwrap();
        store.register("kb", &source).unwrap();
        store.reconcile("kb").unwrap();
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        store
            .attach_embedder(Box::new(Slow::new(gate.clone())))
            .unwrap();
        let coordinator = Coordinator::new(store, None);
        fs::write(&path, "Replacement policy.").unwrap();
        let worker = coordinator.clone();
        let (tx, rx) = mpsc::channel();
        let join = std::thread::spawn(move || {
            tx.send(call(
                &worker,
                json!({"operation":"reconcile","collection":"kb"}),
            ))
            .unwrap()
        });
        gate.wait();
        if replacement == "fifo" {
            fs::remove_file(&path).unwrap();
            let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: creates a special-file fixture only inside this TempDir.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        } else {
            fs::rename(source.join("sub"), temp.path().join("moved")).unwrap();
            let outside = temp.path().join("outside");
            fs::create_dir(&outside).unwrap();
            fs::write(
                outside.join("guide.md"),
                "Outside policy must never be ingested.",
            )
            .unwrap();
            symlink(&outside, source.join("sub")).unwrap();
        }
        gate.release();
        let result = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("special-file replacement must not hang");
        assert_eq!(result.status, "ok");
        let report = result.data.unwrap();
        assert_eq!(report["active"], 0);
        assert_eq!(report["failures"][0]["code"], "source_unavailable");
        assert!(
            search(&coordinator).data.unwrap()["evidence"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            call(&coordinator, json!({"operation":"integrity"})).status,
            "ok"
        );
        join.join().unwrap();
    }
}
