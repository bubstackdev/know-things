//! Atomic publication that never replaces a concurrent destination.
use crate::contracts::{Error, Result};
use std::path::Path;

pub(crate) fn sync_directories(root: &Path) -> Result<()> {
    let mut directories = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry =
            entry.map_err(|_| Error::new("io_error", "Cannot synchronize staged directories"))?;
        if entry.file_type().is_dir() {
            directories.push(entry.into_path());
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        std::fs::File::open(directory)?.sync_all()?;
    }
    Ok(())
}

pub(crate) fn publish_directory(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let source = CString::new(source.as_os_str().as_bytes())
            .map_err(|_| Error::new("invalid_request", "Invalid publication path"))?;
        let destination = CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| Error::new("invalid_request", "Invalid publication path"))?;
        // SAFETY: both paths are NUL-terminated and live through this call.
        // The exclusive flag is part of the same atomic rename operation.
        #[cfg(target_os = "macos")]
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (source, destination);
        Err(Error::new(
            "unsupported_capability",
            "Exclusive directory publication is unavailable on this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{MetadataExt, symlink},
    };
    #[test]
    fn concurrent_empty_directories_files_and_symlinks_are_never_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("asset"), "verified bytes").unwrap();
        let directory = temp.path().join("directory");
        fs::create_dir(&directory).unwrap();
        let inode = fs::metadata(&directory).unwrap().ino();
        assert!(publish_directory(&staging, &directory).is_err());
        assert_eq!(fs::metadata(&directory).unwrap().ino(), inode);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        let file = temp.path().join("file");
        fs::write(&file, "existing user bytes").unwrap();
        assert!(publish_directory(&staging, &file).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "existing user bytes");
        let link = temp.path().join("link");
        symlink(temp.path().join("missing"), &link).unwrap();
        assert!(publish_directory(&staging, &link).is_err());
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let destination = temp.path().join("new");
        publish_directory(&staging, &destination).unwrap();
        assert!(!staging.exists());
        assert_eq!(
            fs::read_to_string(destination.join("asset")).unwrap(),
            "verified bytes"
        );
    }
}
