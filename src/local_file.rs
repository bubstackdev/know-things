use anyhow::{Context, Result, anyhow, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    Rename,
    Sync,
}

pub fn write_atomic(
    path: &Path,
    mode: u32,
    durability: Durability,
    write: impl FnOnce(&mut File) -> Result<()>,
) -> Result<()> {
    let parent = path.parent().context("file requires a parent directory")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    write(temp.as_file_mut())?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    if durability == Durability::Sync {
        temp.as_file().sync_all()?;
    }
    temp.persist(path)?;
    if durability == Durability::Sync {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

pub fn write_json(path: &Path, value: &impl Serialize, max: u64, mode: u32) -> Result<()> {
    write_atomic(path, mode, Durability::Sync, |file| {
        let mut writer = BufWriter::new(&mut *file);
        serde_json::to_writer(&mut writer, value)?;
        writer.flush()?;
        ensure!(
            writer.get_ref().metadata()?.len() <= max,
            "JSON exceeds size limit"
        );
        Ok(())
    })
}

pub fn read_bytes(path: &Path, max: usize) -> Result<Vec<u8>> {
    read_limited(File::open(path)?, max)
}

pub fn read_limited(reader: impl Read, max: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(max as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max, "file exceeds size limit");
    Ok(bytes)
}

pub fn open_lock(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?)
}

pub fn read_json<T: DeserializeOwned>(path: &Path, max: u64) -> Result<T> {
    let file = File::open(path)?;
    ensure!(file.metadata()?.len() <= max, "JSON exceeds size limit");
    let mut input = BufReader::new(file.take(max + 1));
    let value = serde_json::from_reader(&mut input).map_err(|error| {
        anyhow!(
            "invalid JSON: {:?} at line {}",
            error.classify(),
            error.line()
        )
    })?;
    ensure!(input.get_ref().limit() != 0, "JSON exceeds size limit");
    Ok(value)
}

pub fn private_directory(path: &Path, create: bool) -> Result<()> {
    if create {
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "expected a real private directory"
    );
    Ok(())
}
