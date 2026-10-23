//! Offline, content-verified asset bundles. Query paths never install/download assets.
use crate::contracts::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path},
};

// A vendored Python/Torch runtime has tens of thousands of verified files.
// This on-disk manifest limit is independent of the 1 MiB IPC frame limit.
pub const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Bundle {
    pub schema_version: u32,
    pub kind: String,
    pub entrypoint: String,
    pub arguments: Vec<String>,
    pub artifacts: BTreeMap<String, String>,
    pub manifest: serde_json::Value,
}
pub fn relative(path: &str) -> Result<&Path> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|p| !matches!(p, Component::Normal(_)))
    {
        return Err(Error::new(
            "model_incompatible",
            "Asset paths must be relative without traversal",
        ));
    }
    Ok(path)
}
pub fn digest(path: &Path) -> Result<String> {
    digest_until(path, None)
}
pub fn digest_until(path: &Path, deadline: Option<std::time::Instant>) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        check_deadline(deadline)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
pub fn verify(root: &Path) -> Result<Bundle> {
    verify_until(root, None)
}
pub fn verify_until(root: &Path, deadline: Option<std::time::Instant>) -> Result<Bundle> {
    check_deadline(deadline)?;
    let path = root.join("bundle.json");
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(Error::new(
            "model_incompatible",
            "Asset manifest exceeds 8 MiB",
        ));
    }
    let bundle: Bundle = serde_json::from_slice(&bytes)?;
    if bundle.schema_version != 1 || bundle.artifacts.is_empty() {
        return Err(Error::new(
            "model_incompatible",
            "Unsupported or empty asset bundle",
        ));
    }
    let canonical = root.canonicalize()?;
    let mut files = std::collections::BTreeSet::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false).min_depth(1) {
        check_deadline(deadline)?;
        let entry =
            entry.map_err(|_| Error::new("model_incompatible", "Cannot enumerate asset bundle"))?;
        if entry.file_type().is_symlink() {
            return Err(Error::new(
                "model_incompatible",
                "Installed bundles cannot contain symlinks",
            ));
        }
        if entry.file_type().is_file() {
            let name = entry
                .path()
                .strip_prefix(root)
                .ok()
                .and_then(Path::to_str)
                .ok_or_else(|| Error::new("model_incompatible", "Invalid asset name"))?;
            if name != "bundle.json" {
                files.insert(name.to_owned());
            }
        }
    }
    if files != bundle.artifacts.keys().cloned().collect() {
        return Err(Error::new(
            "model_incompatible",
            "Bundle contains unlisted or missing assets",
        ));
    }

    for (name, expected) in &bundle.artifacts {
        let path = root.join(relative(name)?);
        if !path.canonicalize()?.starts_with(&canonical)
            || !path.is_file()
            || expected.len() != 64
            || digest_until(&path, deadline)? != *expected
        {
            return Err(Error::new(
                "model_incompatible",
                "Asset hash or path verification failed",
            ));
        }
    }
    if !bundle.artifacts.contains_key(&bundle.entrypoint) {
        return Err(Error::new(
            "model_incompatible",
            "Entrypoint must be a verified artifact",
        ));
    }
    Ok(bundle)
}
pub fn import(source: &Path, destination: &Path) -> Result<()> {
    let bundle = verify(source)?;
    // Never replace an installed bundle. Incomplete staging is kept out of the final path.
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(Error::new(
                "invalid_request",
                "Asset destination already exists",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = destination
        .parent()
        .filter(|p| p.is_dir())
        .ok_or_else(|| Error::new("invalid_request", "Asset destination parent must exist"))?;
    let staging = parent.join(format!(
        ".install-{}-{}",
        std::process::id(),
        crate::hash(destination.as_os_str().as_encoded_bytes())
    ));
    fs::create_dir(&staging)?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
    let result = (|| -> Result<()> {
        for name in bundle
            .artifacts
            .keys()
            .chain(std::iter::once(&"bundle.json".to_owned()))
        {
            let relative = relative(name)?;
            let target = staging.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut from = File::open(source.join(relative))?;
            let executable = fs::metadata(source.join(relative))?.permissions().mode() & 0o111 != 0;
            let mut to = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(if executable { 0o700 } else { 0o600 })
                .open(target)?;
            std::io::copy(&mut from, &mut to)?;
            to.flush()?;
            to.sync_all()?;
        }
        if verify(&staging)? != bundle {
            return Err(Error::new(
                "model_incompatible",
                "Asset bundle changed during installation",
            ));
        }
        crate::filesystem::sync_directories(&staging)?;
        crate::filesystem::publish_directory(&staging, destination)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn check_deadline(deadline: Option<std::time::Instant>) -> Result<()> {
    if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
        Err(Error::new(
            "deadline_exceeded",
            "Asset verification deadline exceeded",
        ))
    } else {
        Ok(())
    }
}
