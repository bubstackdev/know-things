//! Local generation boundary. Fixtures implement this trait only in tests.
use crate::{
    contracts::*,
    flash::{self, FlashWorker},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
pub trait Cancel: Send + Sync {
    fn cancel(&self) -> Result<()>;
}
impl Cancel for flash::Cancellation {
    fn cancel(&self) -> Result<()> {
        self.cancel()
    }
}
#[derive(Debug)]
pub struct Generated {
    pub finish_reason: String,
    pub generated_tokens: usize,
    pub visible_tokens: usize,
    pub hidden_tokens: usize,
    pub prefill_ms: f64,
    pub total_ms: f64,
    pub cached_prefix_tokens: Option<usize>,
    pub prefix_restore_ms: Option<f64>,
}
pub trait Generator: Send {
    fn identity(&self) -> &GenerationIdentity;
    fn render(&self, system: &str, user: &str) -> Result<String>;
    fn count(&mut self, text: &str, deadline: Instant) -> Result<usize>;
    /// Deltas contain visible answer text only. Every generated token, including
    /// hidden reasoning and its delimiters, contributes to the completion limit.
    fn generate(
        &mut self,
        prompt: &str,
        prompt_tokens: usize,
        limit: usize,
        deadline: Instant,
        started: &mut dyn FnMut(Arc<dyn Cancel>),
        delta: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<Generated>;
    /// Quarantine uncertain state. A subsequent request may create a fresh instance.
    fn invalidate(&mut self);
}
pub struct NativeGenerator {
    worker: Option<FlashWorker>,
    profile: flash::Profile,
    identity: GenerationIdentity,
    worker_root: PathBuf,
    profile_root: PathBuf,
}
impl NativeGenerator {
    pub fn open(worker_root: &Path, profile_root: &Path, deadline: Instant) -> Result<Self> {
        let mut worker = FlashWorker::open_until(worker_root, deadline)?;
        if !worker.has_capability("visible_token_metrics_v1") {
            worker.stop();
            return Err(Error::new(
                "model_incompatible",
                "Worker must filter reasoning and report visible/hidden token counts",
            ));
        }
        let profile = match worker.load_profile_until(profile_root, deadline) {
            Ok(profile) => profile,
            Err(error) => {
                worker.stop();
                return Err(error);
            }
        };
        if profile.architecture == "dense"
            && worker.has_capability("static_system_prefix_v1")
            && let Err(error) = worker.warm_system_prefix(
                &profile.system_prefix(crate::coordinator::SYSTEM)?,
                deadline,
            )
        {
            worker.stop();
            return Err(error);
        }
        let identity = GenerationIdentity {
            profile_id: worker.profile_id().unwrap().to_owned(),
            name: profile.name.clone(),
            tokenizer_id: profile.tokenizer_id.clone(),
            context_limit: profile.context_limit,
            worker_build: worker.worker_build().to_owned(),
            flash_bundle_id: worker.bundle_id().to_owned(),
        };
        Ok(Self {
            worker: Some(worker),
            profile,
            identity,
            worker_root: worker_root.into(),
            profile_root: profile_root.into(),
        })
    }
    fn ready(&mut self, deadline: Instant) -> Result<&mut FlashWorker> {
        if self.worker.is_none() {
            let mut fresh = Self::open(&self.worker_root, &self.profile_root, deadline)?;
            if fresh.identity.profile_id != self.identity.profile_id {
                fresh.invalidate();
                return Err(Error::new(
                    "model_incompatible",
                    "Installed generation profile changed; restart with the new profile explicitly",
                ));
            }
            self.worker = fresh.worker;
        }
        Ok(self.worker.as_mut().unwrap())
    }
}
impl Generator for NativeGenerator {
    fn identity(&self) -> &GenerationIdentity {
        &self.identity
    }
    fn render(&self, system: &str, user: &str) -> Result<String> {
        self.profile.render(system, user)
    }
    fn count(&mut self, text: &str, deadline: Instant) -> Result<usize> {
        let result = self.ready(deadline)?.token_count(text, deadline);
        if result.is_err() {
            self.invalidate();
        }
        result
    }
    fn generate(
        &mut self,
        prompt: &str,
        prompt_tokens: usize,
        limit: usize,
        deadline: Instant,
        started: &mut dyn FnMut(Arc<dyn Cancel>),
        delta: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<Generated> {
        let result = self.ready(deadline)?.generate_with_start(
            prompt,
            prompt_tokens,
            limit,
            deadline,
            |handle| started(Arc::new(handle)),
            delta,
        );
        match result {
            Ok(c) => {
                let (Some(visible), Some(hidden)) = (c.visible_tokens, c.hidden_tokens) else {
                    self.invalidate();
                    return Err(Error::new("worker_failed", "Worker omitted token metrics"));
                };
                if visible + hidden != c.generated_tokens {
                    self.invalidate();
                    return Err(Error::new("worker_failed", "Worker token metrics disagree"));
                }
                Ok(Generated {
                    finish_reason: c.finish_reason,
                    generated_tokens: c.generated_tokens,
                    visible_tokens: visible,
                    hidden_tokens: hidden,
                    prefill_ms: c.prefill_ms,
                    total_ms: c.total_ms,
                    cached_prefix_tokens: c.cached_prefix_tokens,
                    prefix_restore_ms: c.prefix_restore_ms,
                })
            }
            Err(error) => {
                self.invalidate();
                Err(error)
            }
        }
    }
    fn invalidate(&mut self) {
        if let Some(mut worker) = self.worker.take() {
            worker.stop();
        }
    }
}
