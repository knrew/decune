use std::{
    env,
    ffi::OsStr,
    fs, io,
    path::{Component, Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub const DOCKER_TESTS_ENV: &str = "DECUNE_DOCKER_TESTS";

static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    pub fn new() -> io::Result<Self> {
        let temp_root = env::temp_dir();
        let process_id = process::id();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);

        for _ in 0..128 {
            let sequence = NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
            let path = temp_root.join(format!("decune-test-{process_id}-{timestamp}-{sequence}"));

            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "failed to create a unique temporary workspace",
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn create_dir(&self, relative_path: impl AsRef<Path>) -> io::Result<PathBuf> {
        let path = self.resolve(relative_path)?;
        fs::create_dir_all(&path)?;
        Ok(path)
    }

    pub fn write_file(
        &self,
        relative_path: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> io::Result<PathBuf> {
        let path = self.resolve(relative_path)?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(&path, contents)?;
        Ok(path)
    }

    fn resolve(&self, relative_path: impl AsRef<Path>) -> io::Result<PathBuf> {
        let relative_path = relative_path.as_ref();

        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fixture paths must stay inside the temporary workspace",
            ));
        }

        Ok(self.path.join(relative_path))
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn docker_tests_enabled() -> bool {
    docker_tests_enabled_from(env::var_os(DOCKER_TESTS_ENV).as_deref())
}

pub fn docker_tests_enabled_from(value: Option<&OsStr>) -> bool {
    matches!(value, Some(value) if value == OsStr::new("1"))
}

pub fn skip_unless_docker_tests_enabled() -> bool {
    if docker_tests_enabled() {
        false
    } else {
        eprintln!("skipped: set {DOCKER_TESTS_ENV}=1 to run Docker integration tests");
        true
    }
}
