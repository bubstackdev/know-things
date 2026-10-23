use crate::contracts::{Error, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub artifacts: std::collections::BTreeMap<String, String>,
    pub tokenizer: String,
    pub architecture: String,
    pub quantization: String,
    pub runtime: String,
    pub query_prefix: String,
    pub document_prefix: String,
    pub pooling: String,
    pub normalization: String,
    pub dimensions: usize,
    pub max_input_tokens: usize,
    pub license: String,
}
impl Manifest {
    pub fn space_id(&self) -> String {
        crate::hash(
            &serde_json::to_vec(&serde_json::to_value(self).expect("serializable manifest"))
                .expect("canonical manifest"),
        )
    }
}
pub trait Embedder: Send {
    fn manifest(&self) -> &Manifest;
    fn embed(&mut self, text: &str, query: bool, deadline: std::time::Instant) -> Result<Vec<f32>>;
}
/// One local runtime shared by foreground queries and bounded ingestion work.
/// The manifest is immutable and readable without waiting for inference.
pub struct SharedEmbedder {
    manifest: Manifest,
    backend: std::sync::Mutex<Box<dyn Embedder>>,
    queries: std::sync::atomic::AtomicUsize,
}
impl SharedEmbedder {
    pub fn new(backend: Box<dyn Embedder>) -> Self {
        Self {
            manifest: backend.manifest().clone(),
            backend: std::sync::Mutex::new(backend),
            queries: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn embed(&self, text: &str, query: bool, deadline: std::time::Instant) -> Result<Vec<f32>> {
        use std::sync::{TryLockError, atomic::Ordering};
        struct Waiting<'a>(&'a std::sync::atomic::AtomicUsize, bool);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                if self.1 {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
        }
        if query {
            self.queries.fetch_add(1, Ordering::SeqCst);
        }
        let _waiting = Waiting(&self.queries, query);
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(Error::new(
                    "deadline_exceeded",
                    "Embedding deadline exceeded",
                ));
            }
            if query || self.queries.load(Ordering::SeqCst) == 0 {
                match self.backend.try_lock() {
                    Ok(mut backend) => return backend.embed(text, query, deadline),
                    Err(TryLockError::Poisoned(_)) => {
                        return Err(Error::new("worker_failed", "Embedding runtime unavailable"));
                    }
                    Err(TryLockError::WouldBlock) => {}
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}
pub fn encode(vector: &[f32], dimensions: usize) -> Result<Vec<u8>> {
    if dimensions == 0
        || dimensions > 65536
        || vector.len() != dimensions
        || vector.iter().any(|x| !x.is_finite())
    {
        return Err(Error::new(
            "model_incompatible",
            "Invalid embedding dimensions or non-finite components",
        ));
    }
    let norm = vector
        .iter()
        .map(|x| (*x as f64).powi(2))
        .sum::<f64>()
        .sqrt();
    if norm == 0.0 {
        return Err(Error::new("model_incompatible", "Zero embedding vector"));
    }
    Ok(vector
        .iter()
        .flat_map(|x| ((*x as f64 / norm) as f32).to_le_bytes())
        .collect())
}
pub fn dot(query: &[u8], candidate: &[u8]) -> Result<f64> {
    if query.len() != candidate.len() || query.is_empty() || !query.len().is_multiple_of(4) {
        return Err(Error::new("store_corrupt", "Invalid stored vector length"));
    }
    let mut value = 0.0;
    for (a, b) in query
        .as_chunks::<4>()
        .0
        .iter()
        .zip(candidate.as_chunks::<4>().0.iter())
    {
        let a = f32::from_le_bytes(*a);
        let b = f32::from_le_bytes(*b);
        if !a.is_finite() || !b.is_finite() {
            return Err(Error::new("store_corrupt", "Non-finite stored vector"));
        }
        value += a as f64 * b as f64;
    }
    Ok(value)
}

/// Verified local process backend. No installation or network operation occurs here.
pub struct LocalEmbedder {
    manifest: Manifest,
    worker: crate::process::Worker,
    sequence: u64,
}
impl LocalEmbedder {
    pub fn open(root: &std::path::Path) -> Result<Self> {
        let bundle = crate::assets::verify(root)?;
        if bundle.kind != "embedding" {
            return Err(Error::new(
                "model_incompatible",
                "Expected an embedding bundle",
            ));
        }
        let manifest: Manifest = serde_json::from_value(bundle.manifest)?;
        if manifest.artifacts != bundle.artifacts {
            return Err(Error::new(
                "model_incompatible",
                "Embedding and bundle artifacts differ",
            ));
        }
        let root = root.canonicalize()?;
        let mut command = std::process::Command::new(root.join(&bundle.entrypoint));
        command
            .args(&bundle.arguments)
            .current_dir(&root)
            .env("KNOW_THINGS_ASSET_ROOT", &root)
            .env("HF_HUB_OFFLINE", "1")
            .env("TRANSFORMERS_OFFLINE", "1")
            .env("HF_HUB_DISABLE_TELEMETRY", "1")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("OMP_NUM_THREADS", "2")
            .env("TOKENIZERS_PARALLELISM", "false");
        let mut worker = crate::process::Worker::spawn(&mut command)?;
        let hello: serde_json::Value = worker.receive(crate::process::Worker::deadline(
            std::time::Duration::from_secs(30),
        ))?;
        if hello["protocolVersion"] != 1
            || hello["kind"] != "embedding"
            || hello["manifestId"] != manifest.space_id()
            || hello["runtime"] != manifest.runtime
        {
            worker.stop();
            return Err(Error::new(
                "model_incompatible",
                "Embedding worker handshake mismatch",
            ));
        }
        Ok(Self {
            manifest,
            worker,
            sequence: 0,
        })
    }
}
impl Embedder for LocalEmbedder {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    fn embed(&mut self, text: &str, query: bool, deadline: std::time::Instant) -> Result<Vec<f32>> {
        self.sequence += 1;
        self.worker.send(&serde_json::json!({"protocolVersion":1,"requestId":self.sequence,"operation":"embed","text":text,"query":query}),deadline)?;
        let result: serde_json::Value = self.worker.receive(deadline)?;
        if result["requestId"] != self.sequence {
            self.worker.stop();
            return Err(Error::new(
                "worker_failed",
                "Embedding response request ID mismatch",
            ));
        }
        if result["status"] != "ok" {
            return Err(Error::new(
                "model_unavailable",
                "Local embedding inference failed",
            ));
        }
        let vector: Vec<f32> = serde_json::from_value(result["vector"].clone())
            .map_err(|_| Error::new("worker_failed", "Invalid embedding response"))?;
        encode(&vector, self.manifest.dimensions)?;
        Ok(vector)
    }
}
