use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const PATH_PREFIX: &str = "shortcut-docs/";

pub fn validate_workspace(workspace: &str) -> Result<()> {
    ensure!(
        !workspace.is_empty()
            && workspace.len() <= 255
            && workspace
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid Shortcut workspace slug"
    );
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Doc {
    pub id: String,
    pub title: String,
    pub updated_at: String,
    pub content_markdown: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotDoc {
    pub doc: Doc,
    pub sha256: String,
}

impl SnapshotDoc {
    pub fn new(doc: Doc) -> Self {
        let sha256 = format!("{:x}", Sha256::digest(doc.content_markdown.as_bytes()));
        Self { doc, sha256 }
    }

    pub fn path(&self) -> String {
        format!("{PATH_PREFIX}{}.md", self.doc.id)
    }

    pub fn citation(&self, workspace: Option<&str>) -> String {
        match workspace {
            Some(workspace) => format!(
                "https://api.app.shortcut.com/api/v4/{workspace}/documents/{}",
                self.doc.id
            ),
            None => self.path(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            Uuid::parse_str(&self.doc.id).is_ok_and(|id| id.to_string() == self.doc.id),
            "invalid Shortcut Doc ID"
        );
        ensure!(
            !self.doc.title.trim().is_empty()
                && self.doc.title.chars().count() <= 256
                && !self.doc.title.chars().any(char::is_control),
            "invalid Shortcut Doc title"
        );
        ensure!(
            self.doc.updated_at.len() <= 40
                && self.doc.updated_at.ends_with('Z')
                && self.doc.updated_at.contains('T')
                && self
                    .doc
                    .updated_at
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b"-T:.Z".contains(&b)),
            "invalid Shortcut Doc update timestamp"
        );
        ensure!(
            !self.doc.content_markdown.trim().is_empty()
                && self.sha256
                    == format!("{:x}", Sha256::digest(self.doc.content_markdown.as_bytes())),
            "Shortcut Doc content is empty or failed its digest check"
        );
        Ok(())
    }
}
