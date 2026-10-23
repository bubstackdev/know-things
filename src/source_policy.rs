//! Literal source exclusions and opt-in reconciliation scheduling.
use crate::contracts::*;
use rusqlite::{Connection, OptionalExtension};
use std::path::{Component, Path};

pub(crate) fn load(connection: &Connection, collection: &str) -> Result<CollectionPolicy> {
    let encoded: Option<String> = connection
        .query_row(
            "SELECT policy FROM collection_policies WHERE collection_id=?1",
            [collection],
            |r| r.get(0),
        )
        .optional()?;
    let policy: CollectionPolicy = serde_json::from_str(
        &encoded.ok_or_else(|| Error::new("store_corrupt", "Collection policy is missing"))?,
    )
    .map_err(|_| Error::new("store_corrupt", "Invalid collection policy"))?;
    let normalized = policy
        .clone()
        .normalized()
        .map_err(|_| Error::new("store_corrupt", "Invalid collection policy"))?;
    if policy != normalized {
        return Err(Error::new(
            "store_corrupt",
            "Collection policy is not normalized",
        ));
    }
    Ok(policy)
}
impl CollectionPolicy {
    pub fn normalized(mut self) -> Result<Self> {
        if let Some(retention) = &self.retention {
            retention.validate()?;
        }
        if self.excluded_paths.len() > 128
            || self.excluded_paths.iter().map(String::len).sum::<usize>() > 16384
            || self
                .reconcile_interval_ms
                .is_some_and(|ms| !(1000..=86400000).contains(&ms))
        {
            return Err(Error::new(
                "invalid_request",
                "Policy permits 128 paths/16384 bytes and reconciliation intervals of 1000–86400000 ms",
            ));
        }
        for path in &self.excluded_paths {
            let components: Vec<_> = Path::new(path).components().collect();
            if path.is_empty()
                || path.len() > 4096
                || path.contains(['\\', '*', '?', '[', ']', '\0'])
                || components
                    .iter()
                    .any(|c| !matches!(c, Component::Normal(_)))
                || components
                    .iter()
                    .map(|c| c.as_os_str().to_str().unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join("/")
                    != *path
            {
                return Err(Error::new(
                    "invalid_request",
                    "Exclusions must be literal normalized relative POSIX paths without wildcards",
                ));
            }
        }
        self.excluded_paths.sort();
        self.excluded_paths.dedup();
        Ok(self)
    }
    pub fn excludes(&self, relative: &str) -> bool {
        self.excluded_paths.iter().any(|rule| {
            relative == rule
                || relative
                    .strip_prefix(rule)
                    .is_some_and(|tail| tail.starts_with('/'))
        })
    }
}
impl RetentionPolicy {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(1..=1000).contains(&self.retain_revisions)
            || !(1000..=86400000).contains(&self.interval_ms)
            || !(1..=1000).contains(&self.max_revisions)
            || !(1..=5000).contains(&self.deadline_ms)
        {
            return Err(Error::new(
                "invalid_request",
                "Retention requires 1–1000 retained/batch revisions, 1000–86400000 ms interval, and 1–5000 ms deadline",
            ));
        }
        Ok(())
    }
}
