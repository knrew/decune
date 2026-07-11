use std::{
    env,
    fs::{self, File, OpenOptions},
    io,
    os::fd::AsRawFd,
    path::PathBuf,
};

use anyhow::{Context, Result};

pub(crate) const DECUNE_DOCKER_RESOURCE_LOCK_ENV: &str = "DECUNE_DOCKER_RESOURCE_LOCK";

pub(crate) struct DockerResourceLock {
    file: Option<File>,
}

impl DockerResourceLock {
    pub(crate) fn acquire_shared_from_env() -> Result<Self> {
        Self::acquire_from_env(libc::LOCK_SH)
    }

    pub(crate) fn acquire_exclusive_from_env() -> Result<Self> {
        Self::acquire_from_env(libc::LOCK_EX)
    }

    fn acquire_from_env(operation: i32) -> Result<Self> {
        let Some(path) = docker_resource_lock_path() else {
            return Ok(Self { file: None });
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create Docker resource lock directory: {}",
                    parent.display()
                )
            })?;
        }

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("Failed to open Docker resource lock: {}", path.display()))?;
        flock(file.as_raw_fd(), operation)
            .with_context(|| format!("Failed to lock Docker resource lock: {}", path.display()))?;

        Ok(Self { file: Some(file) })
    }
}

impl Drop for DockerResourceLock {
    fn drop(&mut self) {
        if let Some(file) = &self.file {
            _ = flock(file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn docker_resource_lock_path() -> Option<PathBuf> {
    let configured_path = env::var_os(DECUNE_DOCKER_RESOURCE_LOCK_ENV).map(PathBuf::from);
    #[cfg(test)]
    {
        configured_path.or_else(|| Some(default_test_lock_path()))
    }
    #[cfg(not(test))]
    {
        configured_path.or_else(default_test_lock_path)
    }
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "all tests in this binary must lock the same inode; unlinking a lock file can split concurrent lockers across different inodes"
)]
fn default_test_lock_path() -> PathBuf {
    env::temp_dir().join("decune-test-docker-resource.lock")
}

#[cfg(not(test))]
const fn default_test_lock_path() -> Option<PathBuf> {
    None
}

fn flock(fd: i32, operation: i32) -> io::Result<()> {
    loop {
        // SAFETY: flock operates on the supplied file descriptor only; callers pass an fd
        // obtained from an open lock file, and OS errors are reported through errno.
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
