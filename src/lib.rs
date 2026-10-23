pub mod assets;
mod citations;
pub mod contracts;
pub mod coordinator;
pub mod embedding;
mod evidence_selection;
mod filesystem;
pub mod flash;
pub mod generation;
mod ingestion;
mod integrity;
pub mod parser;
mod periodic;
pub mod process;
mod source_policy;
pub mod store;
pub mod transport;
mod vector_store;
mod watch;

use sha2::{Digest, Sha256};
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn identity(parts: &[&str]) -> String {
    // Length framing prevents ambiguity between adjacent fields.
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_be_bytes());
        h.update(p.as_bytes());
    }
    format!("{:x}", h.finalize())
}

mod evidence_render;
