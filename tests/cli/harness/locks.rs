use anyhow::Context;
use std::{fs::File, io, os::fd::AsRawFd, path::PathBuf};

pub(super) struct DockerResourceLock {
    file: File,
}

impl Drop for DockerResourceLock {
    fn drop(&mut self) {
        _ = flock(self.file.as_raw_fd(), libc::LOCK_UN);
    }
}

pub(super) fn acquire_shared_docker_resource_lock() -> anyhow::Result<DockerResourceLock> {
    acquire_docker_resource_lock(libc::LOCK_SH)
}

pub(super) fn acquire_exclusive_docker_resource_lock() -> anyhow::Result<DockerResourceLock> {
    acquire_docker_resource_lock(libc::LOCK_EX)
}

fn acquire_docker_resource_lock(operation: i32) -> anyhow::Result<DockerResourceLock> {
    let path = docker_resource_lock_path();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("Failed to open Docker resource lock: {}", path.display()))?;
    flock(file.as_raw_fd(), operation)
        .with_context(|| format!("Failed to lock Docker resource lock: {}", path.display()))?;

    Ok(DockerResourceLock { file })
}

#[expect(
    clippy::disallowed_methods,
    reason = "all CLI tests and their child processes must lock the same inode; unlinking a lock file can split concurrent lockers across different inodes"
)]
pub(super) fn docker_resource_lock_path() -> PathBuf {
    std::env::temp_dir().join("decune-cli-test-docker-resource.lock")
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
