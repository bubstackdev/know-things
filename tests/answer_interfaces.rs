//! Complete CLI/MCP process tests against an explicitly provisioned protocol fixture.
//! This executable is never selected by the application unless passed as a bundle.
use know_things::{assets, flash::Profile, store::Store};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::Duration,
};
fn bundles(root: &Path) -> (PathBuf, PathBuf) {
    bundles_with_fault(root, None)
}

#[test]
fn answer_evaluation_captures_real_interface_citations_and_local_abstention() {
    let temporary = tempfile::tempdir().unwrap();
    let (worker, profile) = bundles(temporary.path());
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    let raw = "Document policy requires Redis.\n";
    fs::write(source.join("guide.md"), raw).unwrap();
    let content_hash = know_things::hash(raw.as_bytes());
    let identity = know_things::hash(format!("guide.md\t{content_hash}\n").as_bytes());
    let manifest = temporary.path().join("manifest.json");
    fs::write(&manifest, serde_json::to_vec(&json!({"schemaVersion":1,"contentManifestSha256":identity,"files":[{"path":"guide.md","sha256":content_hash,"bytes":raw.len()}]})).unwrap()).unwrap();
    let suite = temporary.path().join("suite.json");
    fs::write(&suite, serde_json::to_vec(&json!({"schemaVersion":1,"corpusManifestSha256":identity,"annotationStatus":"fixture","questions":[
        {"id":"policy","split":"held_out","query":"Document policy","classes":["routine"],"answerable":true,"claims":[{"text":"Redis is required","support":[{"path":"guide.md","contentHash":content_hash,"byteStart":0,"byteEnd":raw.len()}]}]},
        {"id":"unknown","split":"held_out","query":"volcanic","classes":["unanswerable"],"answerable":false,"claims":[]}
    ]})).unwrap()).unwrap();
    let capture = temporary.path().join("capture.json");
    let output = Command::new("python3")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/evaluate_answers.py"))
        .args(["--binary", env!("CARGO_BIN_EXE_know-things")])
        .arg("--source")
        .arg(&source)
        .arg("--manifest")
        .arg(&manifest)
        .arg("--suite")
        .arg(&suite)
        .arg("--store")
        .arg(temporary.path().join("data"))
        .arg("--output")
        .arg(&capture)
        .arg("--flash-bundle")
        .arg(&worker)
        .arg("--generation-profile")
        .arg(&profile)
        .args([
            "--profile",
            "fixture-native",
            "--mode",
            "lexical",
            "--allow-draft",
            "--deadline-ms",
            "5000",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
    assert_eq!(report["questionCount"], 2);
    assert_eq!(report["failures"], 0);
    assert_eq!(report["sourceUnchanged"], true);
    assert_eq!(report["releaseQualified"], false);
    let answer = &report["questions"][0]["answer"];
    assert_eq!(answer["citations"][0]["label"], "S1");
    assert_eq!(answer["semanticValidation"], "not_evaluated");
    assert_eq!(
        report["questions"][1]["answer"]["finishReason"],
        "insufficient_evidence"
    );
    assert_eq!(report["questions"][1]["answer"]["generatedTokens"], 0);
    assert_eq!(
        fs::metadata(&capture).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read_to_string(source.join("guide.md")).unwrap(), raw);
    let template = temporary.path().join("review.json");
    let output = Command::new("python3")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/score_answers.py"))
        .arg("template")
        .arg("--capture")
        .arg(&capture)
        .arg("--output")
        .arg(&template)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let review: Value = serde_json::from_slice(&fs::read(template).unwrap()).unwrap();
    assert_eq!(review["reviewStatus"], "unreviewed");
    assert!(review["questions"][0]["correct"].is_null());
    assert_eq!(review["questions"].as_array().unwrap().len(), 2);

    // Exercise the scorer CLI with explicitly synthetic judgments. The fixture
    // answer does not state the required Redis fact, so completeness is false.
    let mut review = review;
    review["reviewStatus"] = json!("fixture");
    review["reviewer"] = json!("Test fixture, not a human reviewer");
    for entry in review["questions"].as_array_mut().unwrap() {
        let abstained = entry["id"] == "unknown";
        entry["correct"] = json!(abstained);
        entry["complete"] = json!(abstained);
        entry["abstained"] = json!(abstained);
        for key in [
            "factualClaims",
            "supportedFactualClaims",
            "citedFactualClaims",
            "citations",
            "supportedCitations",
        ] {
            entry[key] = json!(if abstained { 0 } else { 1 });
        }
    }
    let fixture_review = temporary.path().join("fixture-review.json");
    fs::write(&fixture_review, serde_json::to_vec(&review).unwrap()).unwrap();
    let scored = temporary.path().join("scored.json");
    let output = Command::new("python3")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/score_answers.py"))
        .arg("score")
        .arg("--capture")
        .arg(&capture)
        .arg("--review")
        .arg(&fixture_review)
        .arg("--output")
        .arg(&scored)
        .arg("--allow-fixture")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let score: Value = serde_json::from_slice(&fs::read(scored).unwrap()).unwrap();
    assert_eq!(score["metrics"]["correctCompleteRate"], 0.0);
    assert_eq!(score["metrics"]["correctAbstentionRate"], 1.0);
    assert_eq!(score["reviewedHeldOut"], false);
    assert_eq!(score["releaseQualified"], false);

    // Missing generation assets produce a recorded failure for every question,
    // rather than aborting the first request or dropping its denominator.
    let missing_capture = temporary.path().join("missing-capture.json");
    let output = Command::new("python3")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/evaluate_answers.py"))
        .args(["--binary", env!("CARGO_BIN_EXE_know-things")])
        .arg("--source")
        .arg(&source)
        .arg("--manifest")
        .arg(&manifest)
        .arg("--suite")
        .arg(&suite)
        .arg("--store")
        .arg(temporary.path().join("missing-data"))
        .arg("--output")
        .arg(&missing_capture)
        .args(["--profile", "missing", "--mode", "lexical", "--allow-draft"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let missing: Value = serde_json::from_slice(&fs::read(missing_capture).unwrap()).unwrap();
    assert_eq!(missing["questionCount"], 2);
    assert_eq!(missing["failures"], 2);
    for question in missing["questions"].as_array().unwrap() {
        assert_eq!(question["errorCode"], "model_unavailable");
        assert!(question["answer"].is_null());
    }
}
fn bundles_with_fault(root: &Path, fault: Option<&str>) -> (PathBuf, PathBuf) {
    let worker = root.join("worker");
    let profile = root.join("profile");
    fs::create_dir(&worker).unwrap();
    fs::create_dir_all(profile.join("model")).unwrap();
    let script = r##"#!/usr/bin/env python3
import os,json,struct,sys,threading,time
cancelled=set()
lock=threading.Lock()
def exact(fd,n):
 b=b''
 while len(b)<n:
  v=os.read(fd,n-len(b))
  if not v:raise EOFError()
  b+=v
 return b
def receive(fd):return json.loads(exact(fd,struct.unpack('>I',exact(fd,4))[0]))
def send(v):
 b=json.dumps(v).encode();sys.stdout.buffer.write(struct.pack('>I',len(b))+b);sys.stdout.buffer.flush()
def control():
 try:
  while True:
   r=receive(int(os.environ['KNOW_THINGS_CONTROL_FD']))
   with lock:cancelled.add(r.get('requestId'))
 except EOFError:os._exit(0)
threading.Thread(target=control,daemon=True).start()
send(dict(protocolVersion=1,kind='flash',workerBuild='fixture-worker',flashArtifactId='fixture-source',memoryGuard=True,capabilities=['tokenize','generate','cancel','health','visible_token_metrics_v1']))
while True:
 try:r=receive(0)
 except EOFError:break
 i=r['requestId'];op=r['operation']
 if op=='load_profile':send(dict(requestId=i,event='profile_loaded',profileId=r['profileId'],contextLimit=r['contextLimit']))
 elif op=='tokenize':send(dict(requestId=i,event='tokenized',tokenCount=len(r['text'].encode())))
 elif op=='generate':
  seq=0;total=0;reason='stop'
  for piece in ['Document policy ', '[S1].']:
   time.sleep(.08)
   with lock:stop=i in cancelled
   if stop:reason='canceled';break
   if total+len(piece.encode())>r['completionLimit']:reason='output_limit';break
   total+=len(piece.encode());seq+=1
   send(dict(requestId=i,sequence=seq,event='delta',text=piece,generatedTokens=total))
  seq+=1
  send(dict(requestId=i,sequence=seq,event='failed' if reason=='canceled' else 'completed',finishReason=reason,promptTokens=r['expectedTokenCount'],generatedTokens=total,visibleTokens=total,hiddenTokens=0,prefillMs=1,totalMs=160,resetComplete=True))
 elif op=='health':send(dict(requestId=i,event='health',loaded=True))
"##;
    let script = if fault == Some("stall_idle") {
        script.replace(
            "contextLimit=r['contextLimit']))",
            "contextLimit=r['contextLimit']));time.sleep(30)",
        )
    } else if let Some(fault) = fault {
        let marker = serde_json::to_string(&root.join("fault-fired")).unwrap();
        let (operation, action) = if fault == "stall_generate" {
            ("generate", "time.sleep(30)")
        } else if fault == "stall_load" {
            ("load_profile", "time.sleep(2)")
        } else {
            (fault, "os._exit(7)")
        };
        script.replace(" i=r['requestId'];op=r['operation']", &format!(" i=r['requestId'];op=r['operation']\n if op=='{operation}' and not os.path.exists({marker}):\n  open({marker},'w').close();{action}"))
    } else {
        script.to_owned()
    };
    fs::write(worker.join("worker.py"), script).unwrap();
    fs::set_permissions(worker.join("worker.py"), fs::Permissions::from_mode(0o700)).unwrap();
    let bundle = json!({"schemaVersion":1,"kind":"flash","entrypoint":"worker.py","arguments":[],"artifacts":{"worker.py":assets::digest(&worker.join("worker.py")).unwrap()},"manifest":{"flashArtifactId":"fixture-source","workerBuild":"fixture-worker"}});
    fs::write(
        worker.join("bundle.json"),
        serde_json::to_vec(&bundle).unwrap(),
    )
    .unwrap();
    fs::write(profile.join("model/config.json"), "{}").unwrap();
    fs::write(profile.join("tokenizer.gguf"), "fixture").unwrap();
    fs::write(profile.join("template.jinja"), "fixture").unwrap();
    let config = Profile {
        name: "fixture-native".into(),
        flash_artifact_id: know_things::hash(&serde_json::to_vec(&bundle).unwrap()),
        architecture: "dense".into(),
        model_path: "model".into(),
        tokenizer_path: "tokenizer.gguf".into(),
        tokenizer_id: assets::digest(&profile.join("tokenizer.gguf")).unwrap(),
        template_path: "template.jinja".into(),
        template_id: assets::digest(&profile.join("template.jinja")).unwrap(),
        template_kind: "qwen3.8-text-no-thinking-v1".into(),
        context_limit: 8192,
        weight_bits: 4,
        license: "fixture".into(),
    };
    fs::write(
        profile.join("profile.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let files: BTreeMap<_, _> = [
        "profile.json",
        "model/config.json",
        "template.jinja",
        "tokenizer.gguf",
    ]
    .into_iter()
    .map(|n| (n, assets::digest(&profile.join(n)).unwrap()))
    .collect();
    fs::write(profile.join("bundle.json"),serde_json::to_vec(&json!({"schemaVersion":1,"kind":"generation_profile","entrypoint":"profile.json","arguments":[],"artifacts":files,"manifest":config})).unwrap()).unwrap();
    (worker, profile)
}
struct Client {
    child: Child,
    input: Option<ChildStdin>,
    lines: Receiver<Value>,
}
impl Client {
    fn spawn(mode: &str, data: &Path, worker: &Path, profile: &Path) -> Self {
        Self::spawn_with_startup(mode, data, worker, profile, None)
    }
    fn spawn_with_startup(
        mode: &str,
        data: &Path,
        worker: &Path,
        profile: &Path,
        deadline: Option<u64>,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_know-things"));
        command.args([
            mode,
            data.to_str().unwrap(),
            "--flash-bundle",
            worker.to_str().unwrap(),
            "--generation-profile",
            profile.to_str().unwrap(),
        ]);
        if let Some(deadline) = deadline {
            command.args(["--generation-startup-deadline-ms", &deadline.to_string()]);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let value = serde_json::from_str(&line).unwrap();
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input: Some(input),
            lines,
        }
    }
    fn send(&mut self, value: Value) {
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{value}").unwrap();
        input.flush().unwrap();
    }
    fn read(&self) -> Value {
        self.lines
            .recv_timeout(Duration::from_secs(5))
            .expect("bounded response")
    }
    fn finish(&mut self) {
        self.input = None;
        let start = std::time::Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let (w, p) = bundles(temp.path());
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("guide.md"), "Document policy requires Redis.").unwrap();
    let data = temp.path().join("data");
    {
        let mut s = Store::open(&data, None).unwrap();
        s.register("kb", &source).unwrap();
        s.reconcile("kb").unwrap();
    }
    (temp, data, w, p)
}
fn request(id: &str, operation: Value) -> Value {
    let mut v = operation;
    v["schemaVersion"] = json!(1);
    v["requestId"] = json!(id);
    v
}
fn answer() -> Value {
    json!({"operation":"answer","question":"policy","collections":["kb"],"profile":"fixture-native","mode":"lexical","deadlineMs":3000})
}
#[test]
fn failed_native_instances_are_recreated_for_the_next_request_without_replay() {
    use know_things::generation::{Generator, NativeGenerator};
    use std::time::Instant;
    for fault in ["tokenize", "generate"] {
        let temp = tempfile::tempdir().unwrap();
        let (worker, profile) = bundles_with_fault(temp.path(), Some(fault));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut generator = NativeGenerator::open(&worker, &profile, deadline).unwrap();
        let identity = generator.identity().profile_id.clone();
        if fault == "tokenize" {
            assert!(generator.count("policy", deadline).is_err());
            assert_eq!(generator.count("policy", deadline).unwrap(), 6);
        } else {
            let mut output = String::new();
            assert!(
                generator
                    .generate("policy", 6, 100, deadline, &mut |_| {}, &mut |s| {
                        output.push_str(s);
                        Ok(())
                    })
                    .is_err()
            );
            assert!(output.is_empty(), "crashed request must not be replayed");
            assert_eq!(generator.count("policy", deadline).unwrap(), 6);
            let stats = generator
                .generate("policy", 6, 100, deadline, &mut |_| {}, &mut |s| {
                    output.push_str(s);
                    Ok(())
                })
                .unwrap();
            assert_eq!(output, "Document policy [S1].");
            assert_eq!(stats.finish_reason, "stop");
        }
        assert_eq!(generator.identity().profile_id, identity);
    }
}
#[test]
fn an_uncooperative_worker_is_stopped_after_cancellation_grace() {
    use know_things::generation::{Generator, NativeGenerator};
    use std::time::Instant;
    let temp = tempfile::tempdir().unwrap();
    let (worker, profile) = bundles_with_fault(temp.path(), Some("stall_generate"));
    let mut generator =
        NativeGenerator::open(&worker, &profile, Instant::now() + Duration::from_secs(5)).unwrap();
    let started = Instant::now();
    let error = generator
        .generate(
            "policy",
            6,
            100,
            started + Duration::from_secs(5),
            &mut |control| control.cancel().unwrap(),
            &mut |_| panic!("canceled request must not deliver text"),
        )
        .unwrap_err();
    assert_eq!(error.code, "canceled");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "cancellation must not wait for the full request deadline"
    );
    assert_eq!(
        generator
            .count("policy", Instant::now() + Duration::from_secs(5))
            .unwrap(),
        6
    );
}

#[test]
fn explicit_generator_invalidation_does_not_wait_for_healthy_shutdown_grace() {
    use know_things::generation::{Generator, NativeGenerator};
    use std::time::Instant;
    let temp = tempfile::tempdir().unwrap();
    let (worker, profile) = bundles_with_fault(temp.path(), Some("stall_idle"));
    let mut generator =
        NativeGenerator::open(&worker, &profile, Instant::now() + Duration::from_secs(5)).unwrap();
    let started = Instant::now();
    generator.invalidate();
    assert!(started.elapsed() < Duration::from_millis(800));
}
#[test]
fn cli_streams_and_accepts_exclusion_while_generation_is_active() {
    let (_temp, data, w, p) = setup();
    let mut c = Client::spawn("local", &data, &w, &p);
    c.send(request("answer", answer()));
    let mut document = None;
    loop {
        let event = c.read();
        if event["data"]["event"] == "evidence_ready" {
            document =
                Some(event["data"]["pack"]["evidence"][0]["reference"]["documentId"].clone());
        }
        if event["data"]["event"] == "answer_delta" {
            break;
        }
    }
    c.send(request(
        "delete",
        json!({"operation":"exclude","document":document.unwrap()}),
    ));
    let mut ack = false;
    let mut failed = false;
    while !ack || !failed {
        let event = c.read();
        if event["requestId"] == "delete" {
            assert_eq!(event["status"], "ok");
            ack = true;
        } else {
            if ack {
                assert_ne!(event["data"]["event"], "answer_delta");
            }
            if event["data"]["event"] == "failed" {
                assert_eq!(event["error"]["code"], "source_unavailable");
                failed = true;
            }
        }
    }
    c.finish();
}
#[test]
fn mcp_progress_and_final_citations_use_the_same_core_stream() {
    let (_temp, data, w, p) = setup();
    let mut c = Client::spawn("mcp-stdio", &data, &w, &p);
    c.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}));
    assert_eq!(c.read()["id"], 1);
    c.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    c.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"know_things","arguments":answer(),"_meta":{"progressToken":"answer-progress"}}}));
    let mut progress = 0;
    let mut saw_text = false;
    loop {
        let message = c.read();
        if message["method"] == "notifications/progress" {
            assert_eq!(message["params"]["progressToken"], "answer-progress");
            let next = message["params"]["progress"].as_u64().unwrap();
            assert!(next > progress);
            progress = next;
            if message["params"]["_meta"]["io.know-things/answerEvent"]["event"] == "answer_delta" {
                saw_text = true;
            }
        } else {
            assert_eq!(message["id"], 2);
            assert_eq!(message["result"]["isError"], false);
            let response: Value =
                serde_json::from_str(message["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(response["data"]["answer"]["referenceValidation"], "valid");
            assert_eq!(response["data"]["answer"]["actualMode"], "lexical");
            break;
        }
    }
    assert!(saw_text);
    c.finish();
}
#[test]
fn mcp_cancellation_is_processed_before_a_blocked_tool_call_finishes() {
    let (_temp, data, w, p) = setup();
    let mut c = Client::spawn("mcp-stdio", &data, &w, &p);
    c.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}));
    c.read();
    c.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    c.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"know_things","arguments":answer(),"_meta":{"progressToken":"p"}}}));
    loop {
        let m = c.read();
        if m["params"]["_meta"]["io.know-things/answerEvent"]["event"] == "answer_delta" {
            break;
        }
    }
    c.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}));
    c.send(json!({"jsonrpc":"2.0","id":3,"method":"ping"}));
    loop {
        let m = c.read();
        assert_ne!(m["id"], 2);
        if m["id"] == 3 {
            break;
        }
    }
    c.finish();
    for m in c.lines.try_iter() {
        assert_ne!(m["id"], 2);
        assert_ne!(
            m["params"]["_meta"]["io.know-things/answerEvent"]["event"],
            "answer_delta"
        );
    }
}

#[test]
fn generation_startup_budget_controls_loading_without_disabling_retrieval() {
    for (deadline, available) in [(1000, false), (5000, true)] {
        let temp = tempfile::tempdir().unwrap();
        let (worker, profile) = bundles_with_fault(temp.path(), Some("stall_load"));
        let mut client = Client::spawn_with_startup(
            "local",
            &temp.path().join("data"),
            &worker,
            &profile,
            Some(deadline),
        );
        client.send(request("status", json!({"operation":"status"})));
        let status = client.read();
        assert_eq!(status["status"], "ok", "{status}");
        assert_eq!(status["data"]["generationAvailable"], available, "{status}");
        if !available {
            assert_eq!(
                status["data"]["generationError"]["code"], "deadline_exceeded",
                "{status}"
            );
        }
        client.send(request("integrity", json!({"operation":"integrity"})));
        assert_eq!(client.read()["status"], "ok");
        client.finish();
    }
}
