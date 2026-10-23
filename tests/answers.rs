//! Controlled generation fixtures prove orchestration; they do not qualify a model.
use know_things::{
    contracts::*,
    coordinator::{Coordinator, Run},
    generation::{Cancel, Generated, Generator},
    store::Store,
};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
struct Gate {
    calls: Mutex<Vec<String>>,
    changed: Condvar,
    allowed: AtomicUsize,
    attempts: AtomicUsize,
    pause_counts: AtomicBool,
    count_entered: AtomicBool,
    count_allowed: AtomicBool,
}
impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            changed: Condvar::new(),
            allowed: AtomicUsize::new(0),
            attempts: AtomicUsize::new(0),
            pause_counts: AtomicBool::new(false),
            count_entered: AtomicBool::new(false),
            count_allowed: AtomicBool::new(false),
        })
    }
    fn wait_calls(&self, n: usize) {
        let start = Instant::now();
        let mut calls = self.calls.lock().unwrap();
        while calls.len() < n {
            assert!(start.elapsed() < Duration::from_secs(3));
            calls = self
                .changed
                .wait_timeout(calls, Duration::from_millis(20))
                .unwrap()
                .0;
        }
    }
    fn release(&self) {
        self.allowed.store(100, Ordering::SeqCst);
        self.changed.notify_all();
    }
}
struct Cancellation(AtomicBool);
impl Cancel for Cancellation {
    fn cancel(&self) -> Result<()> {
        self.0.store(true, Ordering::SeqCst);
        Ok(())
    }
}
struct Fixture {
    identity: GenerationIdentity,
    gate: Arc<Gate>,
    tail: String,
    burst: bool,
}
impl Fixture {
    fn new(gate: Arc<Gate>, tail: &str, burst: bool) -> Self {
        Self {
            identity: GenerationIdentity {
                profile_id: "fixture-profile".into(),
                name: "fixture".into(),
                tokenizer_id: "fixture-utf8-bytes".into(),
                context_limit: 8192,
                worker_build: "fixture-worker".into(),
                flash_bundle_id: "fixture-bundle".into(),
            },
            gate,
            tail: tail.into(),
            burst,
        }
    }
}
impl Generator for Fixture {
    fn identity(&self) -> &GenerationIdentity {
        &self.identity
    }
    fn render(&self, s: &str, u: &str) -> Result<String> {
        Ok(format!("{s}\n{u}"))
    }
    fn count(&mut self, text: &str, deadline: Instant) -> Result<usize> {
        if self.gate.pause_counts.load(Ordering::SeqCst)
            && !self.gate.count_entered.swap(true, Ordering::SeqCst)
        {
            while !self.gate.count_allowed.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    return Err(Error::new("deadline_exceeded", "fixture deadline"));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        if Instant::now() >= deadline {
            return Err(Error::new("deadline_exceeded", "fixture deadline"));
        }
        Ok(text.len())
    }
    fn generate(
        &mut self,
        prompt: &str,
        _tokens: usize,
        _limit: usize,
        deadline: Instant,
        started: &mut dyn FnMut(Arc<dyn Cancel>),
        delta: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<Generated> {
        let canceled = Arc::new(Cancellation(AtomicBool::new(false)));
        started(canceled.clone());
        let index = {
            let mut c = self.gate.calls.lock().unwrap();
            c.push(prompt.into());
            self.gate.changed.notify_all();
            c.len()
        };
        delta("Document policy ")?;
        while self.gate.allowed.load(Ordering::SeqCst) < index {
            if canceled.0.load(Ordering::SeqCst) {
                return Err(Error::new("canceled", "fixture canceled"));
            }
            if Instant::now() >= deadline {
                return Err(Error::new("deadline_exceeded", "fixture deadline"));
            }
            let guard = self.gate.calls.lock().unwrap();
            drop(
                self.gate
                    .changed
                    .wait_timeout(guard, Duration::from_millis(5))
                    .unwrap(),
            );
        }
        if self.burst {
            for _ in 0..100 {
                self.gate.attempts.fetch_add(1, Ordering::SeqCst);
                delta("more ")?;
            }
        }
        delta(&self.tail)?;
        Ok(Generated {
            finish_reason: "stop".into(),
            generated_tokens: 5,
            visible_tokens: 3,
            hidden_tokens: 2,
            prefill_ms: 1.,
            total_ms: 2.,
            cached_prefix_tokens: None,
            prefix_restore_ms: None,
        })
    }
    fn invalidate(&mut self) {}
}
fn answer(question: &str) -> Answer {
    serde_json::from_value(json!({"question":question,"collections":["kb"],"profile":"fixture","mode":"lexical","deadlineMs":3000,"queueDeadlineMs":2000})).unwrap()
}
fn setup(tail: &str, burst: bool) -> (tempfile::TempDir, Coordinator, Arc<Gate>) {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("guide.md"),
        "# Cache\n\nDocument policy requires Redis.\n",
    )
    .unwrap();
    let mut store = Store::open(&temp.path().join("data"), None).unwrap();
    store.register("kb", &source).unwrap();
    store.reconcile("kb").unwrap();
    let gate = Gate::new();
    let coordinator = Coordinator::new(
        store,
        Some(Box::new(Fixture::new(gate.clone(), tail, burst))),
    );
    (temp, coordinator, gate)
}
fn op(coordinator: &Coordinator, value: Value) -> Response {
    let mut value = value;
    value["schemaVersion"] = json!(1);
    value["requestId"] = json!("operation");
    coordinator.execute(serde_json::from_value(value).unwrap())
}
fn drain(run: &mut Run) -> Vec<Response> {
    let mut events = Vec::new();
    while run
        .forward_next(&mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap()
    {}
    events
}
fn to_delta(run: &mut Run) -> Vec<Response> {
    let mut events = Vec::new();
    loop {
        let mut delta = false;
        let more = run
            .forward_next(&mut |e| {
                delta = e
                    .data
                    .as_ref()
                    .is_some_and(|d| d["event"] == "answer_delta");
                events.push(e);
                Ok(())
            })
            .unwrap();
        if delta || !more {
            break;
        }
    }
    events
}
#[test]
fn answers_have_ordered_events_exact_citations_and_explicit_semantic_status() {
    let (_t, c, g) = setup("[S1].", false);
    g.release();
    let mut run = c.start_answer("answer", answer("policy")).unwrap();
    let events = drain(&mut run);
    let names: Vec<_> = events
        .iter()
        .map(|e| e.data.as_ref().unwrap()["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "accepted",
            "evidence_ready",
            "answer_delta",
            "answer_delta",
            "completed"
        ]
    );
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e.request_id, "answer");
        assert_eq!(e.data.as_ref().unwrap()["sequence"], i + 1);
    }
    let result = &events.last().unwrap().data.as_ref().unwrap()["answer"];
    assert_eq!(result["referenceValidation"], "valid");
    assert_eq!(result["semanticValidation"], "not_evaluated");
    assert_eq!(result["generatedTokens"], 5);
    assert_eq!(result["hiddenTokens"], 2);
    let reference = result["citations"][0]["reference"].clone();
    let resolved = op(&c, json!({"operation":"context","reference":reference}));
    assert_eq!(resolved.status, "ok");
    assert!(
        resolved.data.unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("policy")
    );
}
#[test]
fn one_active_one_fifo_waiter_and_explicit_busy() {
    let (_t, c, g) = setup("[S1].", false);
    let mut first = c.start_answer("one", answer("policy first")).unwrap();
    g.wait_calls(1);
    let mut second = c.start_answer("two", answer("policy second")).unwrap();
    let error = c
        .start_answer("three", answer("policy third"))
        .err()
        .unwrap();
    assert_eq!(error.code, "busy");
    assert_eq!(error.details.unwrap()["queued"], 1);
    let mut no_queue = answer("policy reject");
    no_queue.queue_policy = QueuePolicy::Reject;
    assert_eq!(
        c.start_answer("reject", no_queue).err().unwrap().code,
        "busy"
    );
    let search = op(
        &c,
        json!({"operation":"search","query":"Redis","collections":["kb"],"mode":"lexical"}),
    );
    assert_eq!(search.status, "ok");
    g.release();
    drain(&mut first);
    let events = drain(&mut second);
    assert_eq!(events[0].data.as_ref().unwrap()["queued"], true);
    let calls = g.calls.lock().unwrap();
    assert!(calls[0].contains("policy first"));
    assert!(calls[1].contains("policy second"));
}
#[test]
fn queued_deadline_expires_without_generation() {
    let (_t, c, g) = setup("[S1].", false);
    let mut first = c.start_answer("one", answer("policy")).unwrap();
    g.wait_calls(1);
    let mut request = answer("policy queue");
    request.queue_deadline_ms = 10;
    let mut second = c.start_answer("two", request).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || tx.send(drain(&mut second)).unwrap());
    let expired = rx.recv_timeout(Duration::from_millis(500));
    g.release();
    drain(&mut first);
    let events = expired.expect("queue deadline must fire while the active job is still running");
    waiter.join().unwrap();
    assert_eq!(
        events.last().unwrap().error.as_ref().unwrap().code,
        "deadline_exceeded"
    );
    assert_eq!(g.calls.lock().unwrap().len(), 1);
}
#[test]
fn exclusion_fences_queued_deltas_and_completion() {
    let (_t, c, g) = setup("[S1].", false);
    let mut run = c.start_answer("one", answer("policy")).unwrap();
    let before = to_delta(&mut run);
    let pack = &before
        .iter()
        .find(|e| e.data.as_ref().unwrap()["event"] == "evidence_ready")
        .unwrap()
        .data
        .as_ref()
        .unwrap()["pack"];
    let document = pack["evidence"][0]["reference"]["documentId"].clone();
    let ack = op(&c, json!({"operation":"exclude","document":document}));
    assert_eq!(ack.status, "ok");
    let after = drain(&mut run);
    assert!(
        after
            .iter()
            .all(|e| e.data.as_ref().unwrap()["event"] == "failed")
    );
    assert_eq!(
        after.last().unwrap().error.as_ref().unwrap().code,
        "source_unavailable"
    );
    assert_eq!(
        after.last().unwrap().data.as_ref().unwrap()["workerStopped"],
        true
    );
    g.release();
}
#[test]
fn exclusion_also_fences_a_finished_answer_not_yet_delivered() {
    let (_t, c, g) = setup("[S1].", false);
    g.release();
    let mut run = c.start_answer("one", answer("policy")).unwrap();
    g.wait_calls(1);
    let started = Instant::now();
    loop {
        let status = op(&c, json!({"operation":"status"}));
        if status.data.unwrap()["generationQueue"]["active"] == 0 {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(2));
    }
    let search = op(
        &c,
        json!({"operation":"search","query":"policy","collections":["kb"],"mode":"lexical"}),
    );
    let document = search.data.unwrap()["evidence"][0]["reference"]["documentId"].clone();
    op(&c, json!({"operation":"exclude","document":document}));
    let events = drain(&mut run);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].data.as_ref().unwrap()["event"], "accepted");
    assert_eq!(events[1].error.as_ref().unwrap().code, "source_unavailable");
}
#[test]
fn cancellation_is_idempotent_and_does_not_start_a_queued_job() {
    let (_t, c, g) = setup("[S1].", false);
    let mut first = c.start_answer("one", answer("policy")).unwrap();
    g.wait_calls(1);
    let mut second = c.start_answer("two", answer("policy queued")).unwrap();
    c.cancel(second.id()).unwrap();
    c.cancel(second.id()).unwrap();
    c.cancel(first.id()).unwrap();
    assert_eq!(
        drain(&mut first)
            .last()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .code,
        "canceled"
    );
    assert_eq!(
        drain(&mut second)
            .last()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .code,
        "canceled"
    );
    assert_eq!(g.calls.lock().unwrap().len(), 1);
}
#[test]
fn a_slow_consumer_is_bounded_and_gets_a_failed_terminal() {
    let (_t, c, g) = setup("[S1].", true);
    g.release();
    let mut run = c.start_answer("one", answer("policy")).unwrap();
    g.wait_calls(1);
    std::thread::sleep(Duration::from_millis(650));
    let events = drain(&mut run);
    assert_eq!(
        events.last().unwrap().error.as_ref().unwrap().code,
        "resource_exhausted"
    );
    assert!(g.attempts.load(Ordering::SeqCst) < 30);
}
#[test]
fn grouped_citations_resolve_each_source_and_deduplicate_repeated_members() {
    for (tail, expected) in [("[S1, S2].", 2), ("[S1,S1].", 1)] {
        let (_t, c, g) = setup(tail, false);
        g.release();
        let mut run = c.start_answer("grouped", answer("Cache policy")).unwrap();
        let events = drain(&mut run);
        let terminal = events.last().unwrap();
        assert_eq!(terminal.status, "ok", "{terminal:?}");
        let result = &terminal.data.as_ref().unwrap()["answer"];
        assert_eq!(result["referenceValidation"], "valid");
        let citations = result["citations"].as_array().unwrap();
        assert_eq!(citations.len(), expected);
        for citation in citations {
            let resolved = op(
                &c,
                json!({"operation":"context","reference":citation["reference"]}),
            );
            assert_eq!(resolved.status, "ok");
            assert!(!resolved.data.unwrap()["text"].as_str().unwrap().is_empty());
        }
    }
}

#[test]
fn invalid_citations_and_inadequate_budgets_never_complete_successfully() {
    for tail in ["[S999].", "[S1, S999].", "[S1, Sx].", "no citation."] {
        let (_t, c, g) = setup(tail, false);
        g.release();
        let mut run = c.start_answer("one", answer("policy")).unwrap();
        let events = drain(&mut run);
        assert_eq!(events.last().unwrap().status, "error");
        assert!(
            events
                .iter()
                .all(|e| e.data.as_ref().unwrap()["event"] != "completed")
        );
    }
    let (_t, c, g) = setup("[S1].", false);
    let mut request = answer("policy");
    request.input_token_budget = 1;
    let mut run = c.start_answer("small", request).unwrap();
    let events = drain(&mut run);
    assert_eq!(
        events.last().unwrap().error.as_ref().unwrap().code,
        "resource_exhausted"
    );
    assert!(g.calls.lock().unwrap().is_empty());
}
#[test]
fn updating_a_source_keeps_an_active_answer_on_its_original_revision() {
    let (t, c, g) = setup("[S1].", false);
    let mut run = c.start_answer("one", answer("policy")).unwrap();
    let before = to_delta(&mut run);
    let reference = before[1].data.as_ref().unwrap()["pack"]["evidence"][0]["reference"].clone();
    fs::write(
        t.path().join("source/guide.md"),
        "# New\n\nA different policy now applies.",
    )
    .unwrap();
    assert_eq!(
        op(&c, json!({"operation":"reconcile","collection":"kb"})).status,
        "ok"
    );
    g.release();
    let after = drain(&mut run);
    assert_eq!(after.last().unwrap().status, "ok");
    assert_eq!(
        after.last().unwrap().data.as_ref().unwrap()["answer"]["citations"][0]["reference"],
        reference
    );
    assert!(
        op(&c, json!({"operation":"context","reference":reference}))
            .data
            .unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("Redis")
    );
}
#[test]
fn empty_evidence_abstains_locally_and_no_generator_is_an_explicit_error() {
    let (_t, c, g) = setup("[S1].", false);
    let mut run = c.start_answer("one", answer("zzzzzznohit")).unwrap();
    let events = drain(&mut run);
    let last = &events.last().unwrap().data.as_ref().unwrap()["answer"];
    assert_eq!(last["finishReason"], "insufficient_evidence");
    assert_eq!(last["generatedTokens"], 0);
    assert!(g.calls.lock().unwrap().is_empty());
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path(), None).unwrap();
    let c = Coordinator::new(store, None);
    assert_eq!(
        c.start_answer("none", answer("policy")).err().unwrap().code,
        "model_unavailable"
    );
}

#[test]
fn exclusion_during_budgeting_is_not_undone_by_reinclusion() {
    let (_temp, c, g) = setup("[S1].", false);
    let search = op(
        &c,
        json!({"operation":"search","query":"policy","collections":["kb"],"mode":"lexical"}),
    );
    let document = search.data.unwrap()["evidence"][0]["reference"]["documentId"].clone();
    g.pause_counts.store(true, Ordering::SeqCst);
    let mut run = c.start_answer("one", answer("policy")).unwrap();
    let started = Instant::now();
    while !g.count_entered.load(Ordering::SeqCst) {
        assert!(started.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        op(&c, json!({"operation":"exclude","document":document})).status,
        "ok"
    );
    assert_eq!(
        op(&c, json!({"operation":"include","document":document})).status,
        "ok"
    );
    assert_eq!(
        op(&c, json!({"operation":"reconcile","collection":"kb"})).status,
        "ok"
    );
    g.count_allowed.store(true, Ordering::SeqCst);
    g.release();
    let events = drain(&mut run);
    assert_eq!(events.len(), 2);
    assert_eq!(
        events.last().unwrap().error.as_ref().unwrap().code,
        "source_unavailable"
    );
    assert!(g.calls.lock().unwrap().is_empty());
}
#[test]
fn exclusion_acknowledgment_waits_for_an_inflight_delivery_write() {
    let (_temp, c, g) = setup("[S1].", false);
    let mut run = c.start_answer("one", answer("policy")).unwrap();
    let search = op(
        &c,
        json!({"operation":"search","query":"policy","collections":["kb"],"mode":"lexical"}),
    );
    let document = search.data.unwrap()["evidence"][0]["reference"]["documentId"].clone();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let writer_trace = trace.clone();
    let writer = std::thread::spawn(move || {
        let mut blocked = false;
        while run
            .forward_next(&mut |event| {
                if event.data.as_ref().unwrap()["event"] == "answer_delta" {
                    if !blocked {
                        blocked = true;
                        entered_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    }
                    writer_trace.lock().unwrap().push("delta");
                }
                Ok(())
            })
            .unwrap()
        {}
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let (ack_tx, ack_rx) = std::sync::mpsc::channel();
    let deleter_trace = trace.clone();
    let deleter = std::thread::spawn(move || {
        let response = op(&c, json!({"operation":"exclude","document":document}));
        assert_eq!(response.status, "ok");
        deleter_trace.lock().unwrap().push("ack");
        ack_tx.send(()).unwrap();
    });
    assert!(ack_rx.recv_timeout(Duration::from_millis(30)).is_err());
    release_tx.send(()).unwrap();
    ack_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    g.release();
    writer.join().unwrap();
    deleter.join().unwrap();
    assert_eq!(*trace.lock().unwrap(), vec!["delta", "ack"]);
}
#[test]
fn canceling_a_waiter_frees_its_slot_before_the_active_job_finishes() {
    let (_temp, c, g) = setup("[S1].", false);
    let mut first = c.start_answer("one", answer("policy")).unwrap();
    g.wait_calls(1);
    for n in 0..4 {
        let mut waiting = c
            .start_answer(&format!("cancel-{n}"), answer("policy canceled"))
            .unwrap();
        c.cancel(waiting.id()).unwrap();
        let terminal = drain(&mut waiting);
        assert_eq!(
            terminal.last().unwrap().error.as_ref().unwrap().code,
            "canceled"
        );
        assert_eq!(
            terminal.last().unwrap().data.as_ref().unwrap()["workerStopped"],
            true
        );
    }
    let mut replacement = c
        .start_answer("replacement", answer("policy replacement"))
        .unwrap();
    assert_eq!(g.calls.lock().unwrap().len(), 1);
    g.release();
    drain(&mut first);
    let terminal = drain(&mut replacement);
    assert_eq!(terminal.last().unwrap().status, "ok");
    assert_eq!(g.calls.lock().unwrap().len(), 2);
}
#[test]
fn an_expired_unread_waiter_does_not_block_a_replacement_request() {
    let (_temp, c, g) = setup("[S1].", false);
    let mut first = c.start_answer("one", answer("policy")).unwrap();
    g.wait_calls(1);
    let mut request = answer("policy expire");
    request.queue_deadline_ms = 10;
    let mut expired = c.start_answer("expire", request).unwrap();
    std::thread::sleep(Duration::from_millis(25));
    let mut replacement = c
        .start_answer("replace", answer("policy replacement"))
        .unwrap();
    assert_eq!(
        drain(&mut expired)
            .last()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .code,
        "deadline_exceeded"
    );
    g.release();
    drain(&mut first);
    assert_eq!(drain(&mut replacement).last().unwrap().status, "ok");
    assert_eq!(g.calls.lock().unwrap().len(), 2);
}

#[test]
fn lexical_rebuild_preserves_an_active_answers_pinned_snapshot_and_citations() {
    let (_temp, c, g) = setup("[S1].", false);
    let mut run = c.start_answer("answer", answer("policy")).unwrap();
    let before = to_delta(&mut run);
    let pack = before
        .iter()
        .find_map(|r| r.data.as_ref().filter(|d| d["event"] == "evidence_ready"))
        .unwrap()["pack"]
        .clone();
    let rebuild = op(&c, json!({"operation":"rebuild_lexical","deadlineMs":5000}));
    assert_eq!(rebuild.status, "ok");
    assert!(rebuild.corpus_generation.unwrap() > pack["corpusGeneration"].as_u64().unwrap());
    g.release();
    let after = drain(&mut run);
    let terminal = after.last().unwrap();
    assert_eq!(terminal.status, "ok");
    let result = &terminal.data.as_ref().unwrap()["answer"];
    assert_eq!(result["corpusGeneration"], pack["corpusGeneration"]);
    assert_eq!(result["evidencePackId"], pack["evidencePackId"]);
    assert_eq!(
        result["citations"][0]["reference"],
        pack["evidence"][0]["reference"]
    );
    assert_eq!(
        op(
            &c,
            json!({"operation":"context","reference":result["citations"][0]["reference"]})
        )
        .status,
        "ok"
    );
}

#[test]
fn pruning_leases_cover_budgeting_streaming_and_completed_but_undelivered_answers() {
    for phase in ["budgeting", "streaming", "completed"] {
        let (temp, c, g) = setup("[S1].", false);
        if phase == "budgeting" {
            g.pause_counts.store(true, Ordering::SeqCst);
        }
        let mut run = c.start_answer("leased", answer("policy")).unwrap();
        let reference = if phase == "budgeting" {
            let deadline = Instant::now() + Duration::from_secs(1);
            while !g.count_entered.load(Ordering::SeqCst) {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            }
            op(&c, json!({"operation":"search","query":"policy","collections":["kb"],"mode":"lexical"})).data.unwrap()["evidence"][0]["reference"].clone()
        } else {
            to_delta(&mut run)[1].data.as_ref().unwrap()["pack"]["evidence"][0]["reference"].clone()
        };
        if phase == "completed" {
            g.release();
            let deadline = Instant::now() + Duration::from_secs(1);
            while op(&c, json!({"operation":"status"})).data.unwrap()["generationQueue"]["active"]
                != 0
            {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        fs::write(temp.path().join("source/guide.md"), "Replacement policy.").unwrap();
        assert_eq!(
            op(&c, json!({"operation":"reconcile","collection":"kb"})).status,
            "ok"
        );
        let report = op(
            &c,
            json!({"operation":"prune","retainRevisions":1,"maxRevisions":1}),
        );
        assert_eq!(report.status, "ok");
        let report = report.data.unwrap();
        assert_eq!(report["revisionsRemoved"], 0, "{phase}");
        assert_eq!(report["leasedRevisionsDeferred"], 1, "{phase}");
        assert_eq!(report["moreRemaining"], true);
        assert_eq!(
            op(&c, json!({"operation":"context","reference":reference})).status,
            "ok"
        );
        g.count_allowed.store(true, Ordering::SeqCst);
        g.release();
        let events = drain(&mut run);
        let terminal = events.last().unwrap();
        assert_eq!(terminal.status, "ok", "{phase}");
        assert_eq!(
            terminal.data.as_ref().unwrap()["answer"]["citations"][0]["reference"],
            reference
        );
        let report = op(
            &c,
            json!({"operation":"prune","retainRevisions":1,"maxRevisions":1}),
        )
        .data
        .unwrap();
        assert_eq!(
            report["revisionsRemoved"], 1,
            "terminal delivery releases the lease"
        );
        assert_eq!(report["moreRemaining"], false);
        assert_eq!(
            op(&c, json!({"operation":"context","reference":reference}))
                .error
                .unwrap()
                .code,
            "evidence_version_unavailable"
        );
        assert_eq!(op(&c, json!({"operation":"integrity"})).status, "ok");
    }
}

#[test]
fn scheduled_retention_defers_answer_evidence_and_reclaims_it_after_delivery() {
    let (temp, c, g) = setup("[S1].", false);
    let mut request = answer("policy");
    request.deadline_ms = 10000;
    let mut run = c.start_answer("leased", request).unwrap();
    let reference =
        to_delta(&mut run)[1].data.as_ref().unwrap()["pack"]["evidence"][0]["reference"].clone();
    let collection = reference["collectionId"].as_str().unwrap();
    fs::write(temp.path().join("source/guide.md"), "Replacement policy.").unwrap();
    assert_eq!(
        op(&c, json!({"operation":"reconcile","collection":"kb"})).status,
        "ok"
    );
    let configured = op(
        &c,
        json!({"operation":"set_collection_policy","collection":"kb","policy":{"retention":{"retainRevisions":1,"intervalMs":1000,"maxRevisions":1}}}),
    );
    assert_eq!(configured.status, "ok");
    let generation = configured.corpus_generation;
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let response = op(&c, json!({"operation":"status"}));
        let status =
            &response.data.as_ref().unwrap()["periodicReconciliation"]["retention"][collection];
        if status["completedPasses"].as_u64().unwrap_or(0) > 0 {
            assert_eq!(status["lastReport"]["revisionsRemoved"], 0);
            assert_eq!(status["lastReport"]["leasedRevisionsDeferred"], 1);
            assert_eq!(
                response.corpus_generation, generation,
                "deferred automatic passes must not advance generation"
            );
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(15));
    }
    g.release();
    assert_eq!(drain(&mut run).last().unwrap().status, "ok");
    loop {
        let context = op(&c, json!({"operation":"context","reference":reference}));
        if let Some(error) = context.error {
            assert_eq!(error.code, "evidence_version_unavailable");
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(15));
    }
    assert_eq!(op(&c, json!({"operation":"integrity"})).status, "ok");
}

#[test]
fn collection_policy_changes_fence_active_output_even_after_policy_relaxation() {
    let (_temp, c, g) = setup("[S1].", false);
    let mut run = c.start_answer("answer", answer("policy")).unwrap();
    to_delta(&mut run);
    let ack = op(
        &c,
        json!({"operation":"set_collection_policy","collection":"kb","policy":{"excludedPaths":["guide.md"]}}),
    );
    assert_eq!(ack.status, "ok");
    assert_eq!(
        op(
            &c,
            json!({"operation":"set_collection_policy","collection":"kb","policy":{}})
        )
        .status,
        "ok"
    );
    assert_eq!(
        op(&c, json!({"operation":"reconcile","collection":"kb"})).status,
        "ok"
    );
    g.release();
    let after = drain(&mut run);
    assert!(
        after
            .iter()
            .all(|r| r.data.as_ref().unwrap()["event"] == "failed")
    );
    assert_eq!(
        after.last().unwrap().error.as_ref().unwrap().code,
        "source_unavailable"
    );
}
