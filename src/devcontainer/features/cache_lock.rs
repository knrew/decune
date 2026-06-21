use std::{
    fs::{self, File, OpenOptions},
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

pub(crate) struct FeatureCacheLock {
    file: File,
}

impl FeatureCacheLock {
    pub(crate) fn acquire_shared(cache_root: &Path) -> Result<Self> {
        Self::acquire(cache_root, libc::LOCK_SH)
    }

    pub(crate) fn acquire_exclusive(cache_root: &Path) -> Result<Self> {
        Self::acquire(cache_root, libc::LOCK_EX)
    }

    #[cfg(test)]
    pub(crate) fn try_acquire_exclusive(cache_root: &Path) -> Result<Option<Self>> {
        Self::try_acquire(cache_root, libc::LOCK_EX)
    }

    fn acquire(cache_root: &Path, operation: i32) -> Result<Self> {
        let path = feature_cache_lock_path(cache_root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create Feature cache lock directory: {}",
                    parent.display()
                )
            })?;
        }
        let file = open_lock_file(&path)?;
        flock(file.as_raw_fd(), operation)
            .with_context(|| format!("Failed to lock Feature cache: {}", path.display()))?;
        Ok(Self { file })
    }

    #[cfg(test)]
    fn try_acquire(cache_root: &Path, operation: i32) -> Result<Option<Self>> {
        let path = feature_cache_lock_path(cache_root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create Feature cache lock directory: {}",
                    parent.display()
                )
            })?;
        }
        let file = open_lock_file(&path)?;
        match flock(file.as_raw_fd(), operation | libc::LOCK_NB) {
            Ok(()) => Ok(Some(Self { file })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error)
                .with_context(|| format!("Failed to lock Feature cache: {}", path.display())),
        }
    }
}

impl Drop for FeatureCacheLock {
    fn drop(&mut self) {
        let _ = flock(self.file.as_raw_fd(), libc::LOCK_UN);
    }
}

fn feature_cache_lock_path(cache_root: &Path) -> PathBuf {
    cache_root
        .parent()
        .map(|parent| parent.join("features.lock"))
        .unwrap_or_else(|| PathBuf::from("features.lock"))
}

fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("Failed to open Feature cache lock: {}", path.display()))
}

fn flock(fd: i32, operation: i32) -> io::Result<()> {
    loop {
        let status = unsafe { libc::flock(fd, operation) };
        if status == 0 {
            return Ok(());
        }

        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
}

#[cfg(test)]
mod tests {
    use super::FeatureCacheLock;

    #[test]
    fn exclusive_lock_is_blocked_by_shared_lock() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("decune/features");
        let _shared = FeatureCacheLock::acquire_shared(&cache).unwrap();

        assert!(
            FeatureCacheLock::try_acquire_exclusive(&cache)
                .unwrap()
                .is_none()
        );
    }
}
