use std::{
    env,
    ffi::OsStr,
    fs, io,
    path::{Component, Path, PathBuf},
};

use tempfile::TempDir;

pub const DOCKER_TESTS_ENV: &str = "DECUNE_DOCKER_TESTS";

#[derive(Debug)]
pub struct TempWorkspace {
    directory: TempDir,
}

impl TempWorkspace {
    pub fn new() -> io::Result<Self> {
        tempfile::Builder::new()
            .prefix("decune-test-")
            .tempdir()
            .map(|directory| Self { directory })
    }

    pub fn path(&self) -> &Path {
        self.directory.path()
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

        Ok(self.path().join(relative_path))
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
