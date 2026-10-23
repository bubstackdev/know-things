//! Structured native Flash artifact handshake. Model-profile qualification follows separately.
use crate::{
    assets,
    contracts::{Error, Result},
    process::{Control, Worker},
};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
pub struct FlashWorker {
    worker: Worker,
    control: Control,
    hello: Value,
    sequence: u64,
    context_limit: Option<usize>,
    bundle_id: String,
    profile_id: Option<String>,
}
impl FlashWorker {
    pub(crate) fn stop(&mut self) {
        self.worker.stop();
    }
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_until(root, Instant::now() + Duration::from_secs(120))
    }
    pub fn open_until(root: &Path, deadline: Instant) -> Result<Self> {
        let bundle = assets::verify_until(root, Some(deadline))?;
        if bundle.kind != "flash" {
            return Err(Error::new(
                "model_incompatible",
                "Expected a Flash worker bundle",
            ));
        }
        let bundle_id = crate::hash(&serde_json::to_vec(&serde_json::to_value(&bundle)?)?);
        let root = root.canonicalize()?;
        let mut command = Command::new(root.join(&bundle.entrypoint));
        command
            .args(&bundle.arguments)
            .current_dir(&root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("KNOW_THINGS_ASSET_ROOT", &root)
            .env("FLASH_LOAD_THREADS", "2");
        let (mut worker, control) = Worker::spawn_with_control(command)?;
        let hello: Value =
            worker.receive(deadline.min(Instant::now() + Duration::from_secs(30)))?;
        if hello["protocolVersion"] != 1
            || hello["kind"] != "flash"
            || hello["flashArtifactId"] != bundle.manifest["flashArtifactId"]
            || hello["workerBuild"] != bundle.manifest["workerBuild"]
            || hello["memoryGuard"] != true
            || !["tokenize", "generate", "cancel", "health"]
                .iter()
                .all(|name| {
                    hello["capabilities"]
                        .as_array()
                        .is_some_and(|caps| caps.iter().any(|cap| cap == name))
                })
        {
            return Err(Error::new(
                "model_incompatible",
                "Flash worker artifact/capability handshake mismatch",
            ));
        }
        Ok(Self {
            worker,
            control,
            hello,
            sequence: 0,
            context_limit: None,
            bundle_id,
            profile_id: None,
        })
    }
    pub fn health(&mut self) -> Result<Value> {
        self.sequence += 1;
        let deadline = Instant::now() + Duration::from_secs(5);
        self.worker.send(
            &json!({"protocolVersion":1,"requestId":self.sequence,"operation":"health"}),
            deadline,
        )?;
        let result: Value = self.worker.receive(deadline)?;
        if result["requestId"] != self.sequence || result["event"] != "health" {
            self.worker.stop();
            return Err(Error::new(
                "worker_failed",
                "Flash health response mismatch",
            ));
        }
        Ok(json!({"bundleId":self.bundle_id,"handshake":self.hello,"health":result}))
    }
    pub fn cancel(&self, request: u64) -> Result<()> {
        self.control.send(
            &json!({"operation":"cancel","requestId":request}),
            Instant::now() + Duration::from_millis(100),
        )
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub flash_artifact_id: String,
    pub architecture: String,
    pub model_path: String,
    pub tokenizer_path: String,
    pub tokenizer_id: String,
    pub template_path: String,
    pub template_id: String,
    pub template_kind: String,
    pub context_limit: usize,
    pub weight_bits: u8,
    pub license: String,
}
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Completion {
    pub request_id: u64,
    pub finish_reason: String,
    pub generated_tokens: usize,
    pub prompt_tokens: usize,
    pub prefill_ms: f64,
    pub total_ms: f64,
    pub visible_tokens: Option<usize>,
    pub hidden_tokens: Option<usize>,
    pub cached_prefix_tokens: Option<usize>,
    pub prefix_restore_ms: Option<f64>,
}
impl FlashWorker {
    pub fn warm_system_prefix(&mut self, text: &str, deadline: Instant) -> Result<()> {
        let expected = self.token_count(text, deadline)?;
        if expected == 0 || expected > 256 {
            return Err(Error::new(
                "model_incompatible",
                "Static prefix exceeds cache bound",
            ));
        }
        self.sequence += 1;
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, 120000);
        self.worker.send(&json!({"protocolVersion":1,"requestId":self.sequence,"operation":"warm_prefix","text":text,"deadlineMs":remaining}),deadline)?;
        let result: Value = self.worker.receive(deadline)?;
        if result["requestId"] != self.sequence
            || result["event"] != "prefix_ready"
            || result["cachedTokens"] != expected
        {
            self.worker.stop();
            return Err(Error::new(
                "worker_failed",
                "Static prefix preparation mismatch",
            ));
        }
        Ok(())
    }
    pub fn load_profile(&mut self, root: &Path) -> Result<Profile> {
        self.load_profile_until(root, Instant::now() + Duration::from_secs(120))
    }
    pub fn load_profile_until(&mut self, root: &Path, deadline: Instant) -> Result<Profile> {
        if self.context_limit.is_some() {
            return Err(Error::new(
                "invalid_request",
                "Profile switching requires a new worker instance",
            ));
        }
        let bundle = assets::verify_until(root, Some(deadline))?;
        if bundle.kind != "generation_profile" {
            return Err(Error::new(
                "model_incompatible",
                "Expected a generation profile bundle",
            ));
        }
        let profile: Profile = serde_json::from_value(bundle.manifest.clone())?;
        if profile.flash_artifact_id != self.bundle_id
            || !matches!(profile.architecture.as_str(), "dense" | "moe")
            || !(256..=32768).contains(&profile.context_limit)
            || !matches!(profile.weight_bits, 0 | 4 | 8)
        {
            return Err(Error::new(
                "model_incompatible",
                "Incompatible generation profile",
            ));
        }
        let root = root.canonicalize()?;
        let model = root.join(assets::relative(&profile.model_path)?);
        let tokenizer = root.join(assets::relative(&profile.tokenizer_path)?);
        let template = root.join(assets::relative(&profile.template_path)?);
        if !model.canonicalize()?.starts_with(&root)
            || !tokenizer.canonicalize()?.starts_with(&root)
            || !template.canonicalize()?.starts_with(&root)
            || !model.exists()
            || !tokenizer.is_file()
            || !template.is_file()
            || assets::digest_until(&tokenizer, Some(deadline))? != profile.tokenizer_id
            || assets::digest_until(&template, Some(deadline))? != profile.template_id
        {
            return Err(Error::new(
                "model_incompatible",
                "Generation tokenizer/template identity mismatch",
            ));
        }
        let profile_id = crate::hash(&serde_json::to_vec(&serde_json::to_value(&bundle)?)?);
        self.sequence += 1;
        self.worker.send(&json!({"protocolVersion":1,"requestId":self.sequence,"operation":"load_profile","profileId":profile_id,"architecture":profile.architecture,"modelPath":model,"tokenizerPath":tokenizer,"contextLimit":profile.context_limit,"weightBits":profile.weight_bits}),deadline)?;
        let result: Value = self.worker.receive(deadline)?;
        if result["requestId"] != self.sequence
            || result["event"] != "profile_loaded"
            || result["profileId"] != profile_id
            || result["contextLimit"] != profile.context_limit
        {
            self.worker.stop();
            return Err(Error::new(
                "model_unavailable",
                "Flash profile loading failed",
            ));
        }
        self.context_limit = Some(profile.context_limit);
        self.profile_id = Some(profile_id);
        Ok(profile)
    }
    pub fn token_count(&mut self, text: &str, deadline: Instant) -> Result<usize> {
        self.sequence += 1;
        self.worker.send(&json!({"protocolVersion":1,"requestId":self.sequence,"operation":"tokenize","text":text}),deadline)?;
        let result: Value = self.worker.receive(deadline)?;
        if result["requestId"] != self.sequence || result["event"] != "tokenized" {
            self.worker.stop();
            return Err(Error::new("worker_failed", "Tokenizer response mismatch"));
        }
        result["tokenCount"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| Error::new("worker_failed", "Invalid tokenizer count"))
    }
    pub fn controller(&self) -> Control {
        self.control.clone()
    }
    pub fn generate(
        &mut self,
        prompt: &str,
        expected_tokens: usize,
        completion_limit: usize,
        deadline: Instant,
        delta: impl FnMut(&str) -> Result<()>,
    ) -> Result<Completion> {
        self.generate_with_start(
            prompt,
            expected_tokens,
            completion_limit,
            deadline,
            |_| {},
            delta,
        )
    }
    /// The callback receives a request-bound control handle before submission;
    /// it remains usable while the generation thread is reading worker events.
    pub fn generate_with_start(
        &mut self,
        prompt: &str,
        expected_tokens: usize,
        completion_limit: usize,
        deadline: Instant,
        started: impl FnOnce(Cancellation),
        mut delta: impl FnMut(&str) -> Result<()>,
    ) -> Result<Completion> {
        let context = self.context_limit.ok_or_else(|| {
            Error::new(
                "model_unavailable",
                "Load a verified generation profile first",
            )
        })?;
        if expected_tokens.saturating_add(completion_limit) > context {
            return Err(Error::new(
                "resource_exhausted",
                "Prompt and completion exceed the profile context limit",
            ));
        }
        if expected_tokens == 0 || completion_limit == 0 || completion_limit > 4096 {
            return Err(Error::new("invalid_request", "Invalid generation budget"));
        }
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(120000) as u64;
        if remaining == 0 {
            return Err(Error::new(
                "deadline_exceeded",
                "Generation deadline exceeded before submission",
            ));
        }
        self.sequence += 1;
        let id = self.sequence;
        let cancellation = Cancellation {
            control: self.control.clone(),
            request_id: id,
            canceled_at: Arc::new(Mutex::new(None)),
        };
        started(cancellation.clone());
        self.worker.send_checked(&json!({"protocolVersion":1,"requestId":id,"operation":"generate","prompt":prompt,"expectedTokenCount":expected_tokens,"completionLimit":completion_limit,"deadlineMs":remaining}),deadline, || cancellation.check_grace())?;
        let result = (|| -> Result<Completion> {
            let mut sequence = 0u64;
            let mut generated = 0u64;
            let mut bytes = 0usize;
            loop {
                let event: Value = self
                    .worker
                    .receive_checked(deadline, || cancellation.check_grace())?;
                let next = event["sequence"].as_u64().unwrap_or(0);
                let tokens = event["generatedTokens"].as_u64().unwrap_or(0);
                if event["requestId"] != id
                    || next != sequence + 1
                    || tokens < generated
                    || tokens > completion_limit as u64
                {
                    return Err(Error::new(
                        "worker_failed",
                        "Generation event identity, sequence, or token bound mismatch",
                    ));
                }
                sequence = next;
                generated = tokens;
                match event["event"].as_str() {
                    Some("delta") => {
                        let text = event["text"]
                            .as_str()
                            .ok_or_else(|| Error::new("worker_failed", "Invalid answer delta"))?;
                        bytes += text.len();
                        if bytes > 1024 * 1024 {
                            return Err(Error::new("resource_exhausted", "Answer exceeds 1 MiB"));
                        }
                        delta(text)?;
                    }
                    Some("completed" | "failed") => {
                        if event["resetComplete"] != true {
                            return Err(Error::new(
                                "worker_failed",
                                "Worker reset was not confirmed",
                            ));
                        }
                        if event["event"] == "failed" {
                            let reason = event["finishReason"].as_str().unwrap_or("worker_failed");
                            let code = match reason {
                                "canceled" => "canceled",
                                "deadline_exceeded" => "deadline_exceeded",
                                "resource_exhausted" => "resource_exhausted",
                                _ => "worker_failed",
                            };
                            return Err(Error::new(code, "Local generation did not complete"));
                        }
                        if event["promptTokens"] != expected_tokens {
                            return Err(Error::new("worker_failed", "Prompt token count mismatch"));
                        }
                        let cached_prefix_tokens = event
                            .get("cachedPrefixTokens")
                            .map(|value| {
                                value
                                    .as_u64()
                                    .and_then(|n| usize::try_from(n).ok())
                                    .filter(|n| *n <= 256 && *n < expected_tokens)
                                    .ok_or_else(|| {
                                        Error::new("worker_failed", "Invalid prefix cache count")
                                    })
                            })
                            .transpose()?;
                        let prefix_restore_ms = event
                            .get("prefixRestoreMs")
                            .map(|value| {
                                value
                                    .as_f64()
                                    .filter(|n| n.is_finite() && *n >= 0.0)
                                    .ok_or_else(|| {
                                        Error::new("worker_failed", "Invalid prefix restore timing")
                                    })
                            })
                            .transpose()?;
                        if cached_prefix_tokens.is_some() != prefix_restore_ms.is_some() {
                            return Err(Error::new(
                                "worker_failed",
                                "Incomplete prefix cache metrics",
                            ));
                        }
                        return Ok(Completion {
                            request_id: id,
                            finish_reason: event["finishReason"]
                                .as_str()
                                .unwrap_or("unknown")
                                .into(),
                            generated_tokens: generated as usize,
                            prompt_tokens: expected_tokens,
                            prefill_ms: event["prefillMs"].as_f64().unwrap_or(0.0),
                            total_ms: event["totalMs"].as_f64().unwrap_or(0.0),
                            visible_tokens: event["visibleTokens"].as_u64().map(|n| n as usize),
                            hidden_tokens: event["hiddenTokens"].as_u64().map(|n| n as usize),
                            cached_prefix_tokens,
                            prefix_restore_ms,
                        });
                    }
                    _ => {
                        return Err(Error::new(
                            "worker_failed",
                            "Unknown mandatory generation event",
                        ));
                    }
                }
            }
        })();
        if result.is_err() {
            // Fence delivery first (the callback has returned), then stop this instance.
            // A fresh worker must be explicitly loaded; no partial output is replayed.
            let _ = self.cancel(id);
            self.worker.stop();
        }
        result
    }
}
impl Profile {
    pub fn system_prefix(&self, system: &str) -> Result<String> {
        // Validate the renderer before constructing a cacheable, data-free prefix.
        self.render(system, "")?;
        Ok(format!(
            "<|im_start|>system\n{}<|im_end|>\n<|im_start|>user\n",
            system.trim()
        ))
    }
    /// Renderer surface for qualification: two text roles, no history/tools/vision, thinking disabled.
    /// The profile's actual Jinja template must be compared during qualification.
    pub fn render(&self, system: &str, user: &str) -> Result<String> {
        if self.template_kind != "qwen3.8-text-no-thinking-v1" {
            return Err(Error::new(
                "model_incompatible",
                "Unsupported chat template renderer",
            ));
        }
        Ok(format!(
            "<|im_start|>system\n{}<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n",
            system.trim(),
            escape_content(user.trim())
        ))
    }
}

pub fn escape_content(text: &str) -> String {
    // Preserve authoritative source bytes in the evidence pack; only the rendered
    // prompt escapes model control markers so source text cannot create chat roles.
    text.replace("<|", "<\u{2060}|")
        .replace("<think>", "<\u{2060}think>")
        .replace("</think>", "<\u{2060}/think>")
}

impl FlashWorker {
    pub fn load_tokenizer_bundle(&mut self, root: &Path) -> Result<()> {
        let bundle = assets::verify(root)?;
        if bundle.kind != "tokenizer" {
            return Err(Error::new(
                "model_incompatible",
                "Expected a tokenizer bundle",
            ));
        }
        let path = root.canonicalize()?.join(&bundle.entrypoint);
        if assets::digest(&path)? != bundle.manifest["tokenizerId"] {
            return Err(Error::new(
                "model_incompatible",
                "Tokenizer bundle identity mismatch",
            ));
        }
        self.sequence += 1;
        let deadline = Instant::now() + Duration::from_secs(30);
        self.worker.send(&json!({"protocolVersion":1,"requestId":self.sequence,"operation":"load_tokenizer","tokenizerPath":path}),deadline)?;
        let result: Value = self.worker.receive(deadline)?;
        if result["requestId"] != self.sequence || result["event"] != "tokenizer_loaded" {
            self.worker.stop();
            return Err(Error::new(
                "model_incompatible",
                "Tokenizer-only loading failed",
            ));
        }
        Ok(())
    }
    pub fn token_ids(&mut self, text: &str, deadline: Instant) -> Result<Vec<i32>> {
        self.sequence += 1;
        self.worker.send(&json!({"protocolVersion":1,"requestId":self.sequence,"operation":"tokenize","text":text,"returnTokens":true}),deadline)?;
        let result: Value = self.worker.receive(deadline)?;
        if result["requestId"] != self.sequence || result["event"] != "tokenized" {
            self.worker.stop();
            return Err(Error::new(
                "worker_failed",
                "Tokenizer identity response mismatch",
            ));
        }
        let ids: Vec<i32> = serde_json::from_value(result["tokenIds"].clone())
            .map_err(|_| Error::new("worker_failed", "Invalid tokenizer IDs"))?;
        if result["tokenCount"] != ids.len() || ids.len() > 65536 || ids.iter().any(|id| *id < 0) {
            self.worker.stop();
            return Err(Error::new("worker_failed", "Tokenizer ID count mismatch"));
        }
        Ok(ids)
    }
}

#[derive(Clone)]
pub struct Cancellation {
    control: Control,
    request_id: u64,
    canceled_at: Arc<Mutex<Option<Instant>>>,
}
impl Cancellation {
    pub fn request_id(&self) -> u64 {
        self.request_id
    }
    pub fn cancel(&self) -> Result<()> {
        let mut at = self
            .canceled_at
            .lock()
            .map_err(|_| Error::new("worker_failed", "Cancellation state unavailable"))?;
        at.get_or_insert_with(Instant::now);
        drop(at);
        self.control.send(
            &json!({"operation":"cancel","requestId":self.request_id}),
            Instant::now() + Duration::from_millis(100),
        )
    }
    fn check_grace(&self) -> Result<()> {
        let at = self
            .canceled_at
            .lock()
            .map_err(|_| Error::new("worker_failed", "Cancellation state unavailable"))?;
        if at.is_some_and(|at| at.elapsed() >= Duration::from_secs(1)) {
            Err(Error::new(
                "canceled",
                "Worker did not stop within cancellation grace",
            ))
        } else {
            Ok(())
        }
    }
}

impl FlashWorker {
    pub fn bundle_id(&self) -> &str {
        &self.bundle_id
    }
    pub fn profile_id(&self) -> Option<&str> {
        self.profile_id.as_deref()
    }
    pub fn worker_build(&self) -> &str {
        self.hello["workerBuild"].as_str().unwrap_or("")
    }
    pub fn has_capability(&self, capability: &str) -> bool {
        self.hello["capabilities"]
            .as_array()
            .is_some_and(|a| a.iter().any(|c| c == capability))
    }
}
